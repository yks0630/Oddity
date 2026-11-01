//! 启动器自身文件的完整性校验.
//!
//! 安装包内带有构建时生成的 `FILES.sha256`, 联网时改用当前版本 Release
//! 的官方清单; AppImage 直接核对整个镜像文件. 这里只负责比对和状态归类,
//! 后台调度与用户提示由 nte-service 处理.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::http::HttpClient;
use crate::{Error, Result};

/// 应用目录内的本地清单文件名.
pub const MANIFEST_NAME: &str = "FILES.sha256";
/// Release 校验和清单文件名.
pub const CHECKSUMS_NAME: &str = "SHA256SUMS";
/// 清单从当前版本 tag 的 Release 资产直链获取.
const REPOSITORY: &str = "LaT-SKY/Oddity";
const TAG_PREFIX: &str = "oddity-v";
/// 清单大小上限, 正常清单只有几十 KiB.
const MAX_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;
/// 清单条目上限, 防止异常文件把后台任务拖住.
const MAX_MANIFEST_ENTRIES: usize = 4096;
/// 状态里最多保留的变更文件数, 其余通过总数提示.
const MAX_CHANGED_FILES: usize = 200;
/// 在线清单的单次请求超时.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum State {
    /// 后台校验尚未完成, 界面可以显示上一次的结果时间.
    Checking,
    /// 在线官方清单比对通过.
    Verified,
    /// 联网失败时使用安装包内的清单比对通过.
    Baseline,
    /// 联网和本地比对都没有完成, 不能当作通过.
    Unverified,
    /// 官方清单存在, 但本地文件缺失或内容不一致.
    Changed,
    /// Nix 等源码构建, 文件与官方产物不同, 不做校验.
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Source {
    /// 当前版本 Release 的官方清单.
    Online,
    /// 安装包内随应用发布的清单.
    Bundled,
}

/// 需要在界面上提示一次的状态变化.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Notice {
    /// 从其他状态变为在线已验证.
    Success,
    /// 当前应用版本第一次发现文件不一致.
    Changed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub state: State,
    pub source: Option<Source>,
    /// 最后一次完成的检查时间 (Unix 毫秒), 0 表示还没有完成过.
    pub checked_at: u64,
    /// 变更文件, 按路径排序; 超过上限时只保留前 200 个.
    pub changed: Vec<String>,
    /// 变更文件总数, 用于说明列表是否被截断.
    pub changed_total: usize,
    /// 本次检查产生的一次性提示, 界面确认后由后端清除.
    pub notice: Option<Notice>,
    pub app_version: String,
    pub release_url: String,
}

impl Status {
    pub fn checking(app_version: &str, checked_at: u64) -> Self {
        Self {
            state: State::Checking,
            source: None,
            checked_at,
            changed: Vec::new(),
            changed_total: 0,
            notice: None,
            app_version: app_version.to_string(),
            release_url: release_url(app_version),
        }
    }
}

/// 当前运行方式; 只有打包产物才能和官方清单比对.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    AppImage,
    Package,
    Nix,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installation {
    pub method: Method,
    /// 解包后的应用目录, AppImage 为挂载目录.
    pub root: PathBuf,
    /// AppImage 文件本身.
    pub image: Option<PathBuf>,
}

impl Installation {
    /// 从当前进程和 AppImage 运行时提供的环境变量推断安装方式.
    pub fn detect() -> Self {
        let image = std::env::var_os("APPIMAGE")
            .map(PathBuf::from)
            .filter(|path| !path.as_os_str().is_empty());
        let executable = std::env::current_exe().ok();
        Self::from_executable(executable.as_deref(), image)
    }

    /// 测试与探测共用的纯路径版本, 不读取环境变量.
    pub fn from_executable(executable: Option<&Path>, image: Option<PathBuf>) -> Self {
        let executable =
            executable.map(|path| fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()));
        let root = executable.as_deref().and_then(app_root_of);
        if executable
            .as_deref()
            .is_some_and(|path| path.starts_with("/nix/store"))
        {
            return Self {
                method: Method::Nix,
                root: root.unwrap_or_default(),
                image: None,
            };
        }
        if let Some(image) = image {
            return Self {
                method: Method::AppImage,
                root: root.unwrap_or_default(),
                image: Some(image),
            };
        }
        match root {
            Some(root) => Self {
                method: Method::Package,
                root,
                image: None,
            },
            None => Self {
                method: Method::Unsupported,
                root: PathBuf::new(),
                image: None,
            },
        }
    }
}

/// 可执行文件位于 `<应用目录>/resources/bin/` 时才能定位到打包目录.
fn app_root_of(executable: &Path) -> Option<PathBuf> {
    let bin = executable.parent()?;
    let resources = bin.parent()?;
    let root = resources.parent()?;
    if bin.file_name()? != "bin" || resources.file_name()? != "resources" {
        return None;
    }
    Some(root.to_path_buf())
}

/// 当前版本 Release 资产的直链前缀.
pub fn release_base(app_version: &str) -> String {
    format!("https://github.com/{REPOSITORY}/releases/download/{TAG_PREFIX}{app_version}")
}

/// 当前版本 Release 页面, 校验失败时引导用户从这里重装.
pub fn release_url(app_version: &str) -> String {
    format!("https://github.com/{REPOSITORY}/releases/tag/{TAG_PREFIX}{app_version}")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex(&hasher.finalize())
}

fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex(&hasher.finalize()))
}

/// GNU `sha256sum` 格式的清单: `<hash>  <path>` 或 `<hash> *<path>`.
fn parse_manifest(text: &str) -> Result<BTreeMap<String, String>> {
    let mut entries = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        if entries.len() >= MAX_MANIFEST_ENTRIES {
            return Err(Error::Parse("完整性清单条目过多".into()));
        }
        let raw = line.as_bytes();
        if raw.len() < 67 || !raw[..64].iter().all(u8::is_ascii_hexdigit) {
            return Err(Error::Parse(format!("完整性清单行格式错误: {line}")));
        }
        if &raw[64..66] != b"  " && &raw[64..66] != b" *" {
            return Err(Error::Parse(format!("完整性清单行格式错误: {line}")));
        }
        // 前 66 个字节都是 ASCII, 这里按字节切分是安全的.
        let path = &line[66..];
        if !valid_manifest_path(path) {
            return Err(Error::Parse(format!("完整性清单路径无效: {path}")));
        }
        let hash = line[..64].to_ascii_lowercase();
        if entries.insert(path.to_string(), hash).is_some() {
            return Err(Error::Parse(format!("完整性清单路径重复: {path}")));
        }
    }
    if entries.is_empty() {
        return Err(Error::Parse("完整性清单为空".into()));
    }
    Ok(entries)
}

fn valid_manifest_path(path: &str) -> bool {
    if path.is_empty() || path.starts_with('/') || path.contains(['\0', '\\']) {
        return false;
    }
    if path
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return false;
    }
    crate::paths::relative_path(path).is_ok()
}

fn load_manifest(root: &Path) -> Result<BTreeMap<String, String>> {
    let path = root.join(MANIFEST_NAME);
    let metadata = fs::metadata(&path)?;
    if metadata.len() > MAX_MANIFEST_BYTES {
        return Err(Error::Parse(format!("完整性清单过大: {}", path.display())));
    }
    let text = fs::read_to_string(path)?;
    parse_manifest(&text)
}

fn fetch_manifest(url: &str, timeout: Duration) -> Result<BTreeMap<String, String>> {
    let bytes = HttpClient::shared().fetch_bytes(url, MAX_MANIFEST_BYTES, timeout)?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| Error::Parse(format!("完整性清单不是 UTF-8: {url}")))?;
    parse_manifest(text)
}

/// 按清单核对应用目录, 返回不一致、缺失或未登记的路径.
///
/// 符号链接按目标文件内容核对; 清单之外的文件也算改动, 但 AppImage 运行时
/// 自己追加的 AppRun、图标链接和 usr 目录不在此列.
fn verify(root: &Path, manifest: &BTreeMap<String, String>, method: Method) -> Result<Vec<String>> {
    let canonical_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut changed = Vec::new();
    for (path, expected) in manifest {
        let relative = crate::paths::relative_path(path)?;
        let full = root.join(relative);
        let inside = fs::canonicalize(&full)
            .map(|target| target.starts_with(&canonical_root))
            .unwrap_or(false);
        let matches = inside
            && fs::metadata(&full)
                .map(|metadata| metadata.is_file())
                .unwrap_or(false)
            && sha256_file(&full)
                .map(|actual| actual.eq_ignore_ascii_case(expected))
                .unwrap_or(false);
        if !matches {
            changed.push(path.clone());
        }
    }

    let mut pending = vec![PathBuf::new()];
    while let Some(relative) = pending.pop() {
        let directory = root.join(&relative);
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let relative = if relative.as_os_str().is_empty() {
                PathBuf::from(&name)
            } else {
                relative.join(&name)
            };
            let Some(text) = relative.to_str() else {
                changed.push(relative.display().to_string());
                continue;
            };
            let text = text.replace(std::path::MAIN_SEPARATOR, "/");
            if text == MANIFEST_NAME || manifest.contains_key(&text) {
                continue;
            }
            let metadata = match fs::symlink_metadata(entry.path()) {
                Ok(metadata) => metadata,
                Err(_) => continue,
            };
            if metadata.is_dir() {
                pending.push(relative);
                continue;
            }
            if allowed_extra(&text, method) {
                continue;
            }
            changed.push(text);
        }
    }
    changed.sort();
    changed.dedup();
    Ok(changed)
}

fn allowed_extra(path: &str, method: Method) -> bool {
    // deb/rpm 打包时 electron-builder 会追加 AppArmor profile, 不属于应用代码.
    if path == "resources/apparmor-profile" {
        return true;
    }
    // AppImage 运行时会在镜像根目录追加启动器和桌面资源.
    method == Method::AppImage
        && (matches!(path, "AppRun" | ".DirIcon" | "oddity.png") || path.starts_with("usr/"))
}

fn truncated(changed: Vec<String>) -> (Vec<String>, usize) {
    let total = changed.len();
    (changed.into_iter().take(MAX_CHANGED_FILES).collect(), total)
}

fn classify(
    root: &Path,
    manifest: &BTreeMap<String, String>,
    source: Source,
    method: Method,
) -> (State, Option<Source>, Vec<String>, usize) {
    match verify(root, manifest, method) {
        Ok(changed) => {
            let (changed, total) = truncated(changed);
            let state = if total == 0 {
                match source {
                    Source::Online => State::Verified,
                    Source::Bundled => State::Baseline,
                }
            } else {
                State::Changed
            };
            (state, Some(source), changed, total)
        }
        Err(_) => (State::Unverified, None, Vec::new(), 0),
    }
}

fn check_package(
    root: &Path,
    base: &str,
    timeout: Duration,
) -> (State, Option<Source>, Vec<String>, usize) {
    if let Ok(manifest) = fetch_manifest(&format!("{base}/{MANIFEST_NAME}"), timeout) {
        return classify(root, &manifest, Source::Online, Method::Package);
    }
    match load_manifest(root) {
        Ok(manifest) => classify(root, &manifest, Source::Bundled, Method::Package),
        Err(_) => (State::Unverified, None, Vec::new(), 0),
    }
}

/// SHA256SUMS 里只有一条 AppImage 资产; 允许用户重命名下载的文件.
fn appimage_entry<'a>(
    sums: &'a BTreeMap<String, String>,
    image: &Path,
) -> Option<(&'a String, &'a String)> {
    if let Some(name) = image.file_name().and_then(|name| name.to_str()) {
        if let Some((key, hash)) = sums.get_key_value(name) {
            return Some((key, hash));
        }
    }
    let mut candidates = sums.iter().filter(|(name, _)| name.ends_with(".AppImage"));
    let (name, hash) = candidates.next()?;
    if candidates.next().is_some() {
        return None;
    }
    Some((name, hash))
}

fn check_appimage(
    installation: &Installation,
    base: &str,
    timeout: Duration,
) -> (State, Option<Source>, Vec<String>, usize) {
    let Some(image) = installation.image.as_deref() else {
        return (State::Unverified, None, Vec::new(), 0);
    };
    let online = fetch_manifest(&format!("{base}/{CHECKSUMS_NAME}"), timeout).and_then(|sums| {
        let (name, expected) = appimage_entry(&sums, image)
            .ok_or_else(|| Error::Parse("校验和里没有 AppImage".into()))?;
        let actual = sha256_file(image)?;
        Ok((name.clone(), actual.eq_ignore_ascii_case(expected)))
    });
    if let Ok((name, matches)) = online {
        return if matches {
            (State::Verified, Some(Source::Online), Vec::new(), 0)
        } else {
            (State::Changed, Some(Source::Online), vec![name], 1)
        };
    }
    if installation.root.as_os_str().is_empty() {
        return (State::Unverified, None, Vec::new(), 0);
    }
    match load_manifest(&installation.root) {
        Ok(manifest) => classify(
            &installation.root,
            &manifest,
            Source::Bundled,
            Method::AppImage,
        ),
        Err(_) => (State::Unverified, None, Vec::new(), 0),
    }
}

/// 检查一次并返回可直接展示的状态.
pub fn check(installation: &Installation, app_version: &str, timeout: Duration) -> Status {
    check_at(
        installation,
        app_version,
        &release_base(app_version),
        timeout,
    )
}

/// 与 [`check`] 相同, 但使用指定的 Release 资产前缀, 便于测试.
pub fn check_at(
    installation: &Installation,
    app_version: &str,
    base: &str,
    timeout: Duration,
) -> Status {
    let (state, source, changed, changed_total) = match installation.method {
        Method::Nix | Method::Unsupported => (State::Unsupported, None, Vec::new(), 0),
        Method::AppImage => check_appimage(installation, base, timeout),
        Method::Package => check_package(&installation.root, base, timeout),
    };
    Status {
        state,
        source,
        checked_at: now_ms(),
        changed,
        changed_total,
        notice: None,
        app_version: app_version.to_string(),
        release_url: release_url(app_version),
    }
}

/// 数据目录内的校验历史, 用于判断状态变化和一次性提示.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct History {
    pub app_version: String,
    pub state: Option<State>,
    pub checked_at: u64,
    /// 已经提示过文件不一致的应用版本.
    pub changed_notice_version: Option<String>,
    /// 尚未被界面确认的提示.
    pub pending_notice: Option<Notice>,
    /// 尚未确认的变更文件, 下次启动恢复提示时继续展示.
    pub pending_changed: Vec<String>,
    /// 尚未确认的变更文件总数.
    pub pending_changed_total: usize,
    /// 发现改动时使用的清单来源.
    pub pending_source: Option<Source>,
}

impl History {
    pub fn load(path: &Path) -> Self {
        fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self)
            .map_err(|error| Error::Parse(format!("序列化校验状态失败: {error}")))?;
        let temporary = path.with_extension("json.tmp");
        fs::write(&temporary, text)?;
        fs::rename(&temporary, path)?;
        Ok(())
    }

    /// 记录一次完成的检查并给出需要提示的变化.
    pub fn observe(&mut self, status: &Status) -> Option<Notice> {
        let previous = self.state;
        let fresh = match status.state {
            // 离线基线通过只更新状态; 只有从未验证或已修改恢复时才提示验证成功.
            State::Verified
                if matches!(
                    previous,
                    None | Some(State::Unverified) | Some(State::Changed)
                ) =>
            {
                Some(Notice::Success)
            }
            // 只有官方清单确认不一致时才引导重装; 网络失败时仅记录状态.
            State::Changed
                if status.source == Some(Source::Online)
                    && self.changed_notice_version.as_deref()
                        != Some(status.app_version.as_str()) =>
            {
                Some(Notice::Changed)
            }
            _ => None,
        };
        let pending = match fresh {
            Some(Notice::Changed) => {
                self.pending_changed = status.changed.clone();
                self.pending_changed_total = status.changed_total;
                self.pending_source = status.source;
                Some(Notice::Changed)
            }
            Some(Notice::Success) => {
                self.pending_changed.clear();
                self.pending_changed_total = 0;
                self.pending_source = None;
                Some(Notice::Success)
            }
            None => {
                // 用户还没确认的改动提示不能因为本次联网失败或没复现而丢掉.
                let preserved = self.pending_notice == Some(Notice::Changed)
                    && self.app_version == status.app_version
                    && status.state != State::Verified
                    && self.changed_notice_version.as_deref() != Some(status.app_version.as_str());
                if !preserved {
                    self.pending_changed.clear();
                    self.pending_changed_total = 0;
                    self.pending_source = None;
                }
                preserved.then_some(Notice::Changed)
            }
        };
        self.app_version = status.app_version.clone();
        self.state = Some(status.state);
        self.checked_at = status.checked_at;
        self.pending_notice = pending;
        pending
    }

    /// 界面确认提示后记录, 同一应用版本不再重复引导重装.
    pub fn acknowledge(&mut self, notice: Notice) -> bool {
        if self.pending_notice != Some(notice) {
            return false;
        }
        if notice == Notice::Changed {
            self.changed_notice_version = Some(self.app_version.clone());
            self.pending_changed.clear();
            self.pending_changed_total = 0;
            self.pending_source = None;
        }
        self.pending_notice = None;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{HttpServer, TempDir};

    fn manifest_line(bytes: &[u8], path: &str) -> String {
        format!("{}  {path}\n", sha256_bytes(bytes))
    }

    fn package_installation(root: &TempDir) -> Installation {
        Installation {
            method: Method::Package,
            root: root.path().to_path_buf(),
            image: None,
        }
    }

    #[test]
    fn parses_sha256sum_output() {
        let parsed = parse_manifest(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  a/b\n\
             bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb *c d\r\n\n",
        )
        .unwrap();
        assert_eq!(parsed["a/b"], "a".repeat(64));
        assert_eq!(parsed["c d"], "b".repeat(64));

        for invalid in [
            "short  a".to_string(),
            format!("{}  ../escape\n", "a".repeat(64)),
            format!("{}  /absolute\n", "a".repeat(64)),
            format!("{}  a//b\n", "a".repeat(64)),
            format!("{}  bad:name\n", "a".repeat(64)),
            format!("{} a\n", "a".repeat(64)),
            format!("{}  a\n{}  a\n", "a".repeat(64), "b".repeat(64)),
            String::new(),
        ] {
            assert!(parse_manifest(&invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn verify_reports_missing_and_modified_files() {
        let root = TempDir::new("integrity-verify");
        root.write("bin/a", b"one");
        root.write("lib/b.txt", b"two");
        let manifest = parse_manifest(&format!(
            "{}{}",
            manifest_line(b"one", "bin/a"),
            manifest_line(b"two", "lib/b.txt")
        ))
        .unwrap();
        assert!(verify(root.path(), &manifest, Method::Package)
            .unwrap()
            .is_empty());

        root.write("lib/b.txt", b"changed");
        root.write("bin/c", b"extra");
        let changed = verify(root.path(), &manifest, Method::Package).unwrap();
        assert_eq!(changed, vec!["bin/c".to_string(), "lib/b.txt".to_string()]);

        fs::remove_file(root.join("bin/a")).unwrap();
        let changed = verify(root.path(), &manifest, Method::Package).unwrap();
        assert_eq!(
            changed,
            vec![
                "bin/a".to_string(),
                "bin/c".to_string(),
                "lib/b.txt".to_string()
            ]
        );
    }

    #[test]
    fn verify_checks_symlink_targets_and_unregistered_files() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new("integrity-symlinks");
        root.write("resources/bin/tool", b"tool");
        symlink("resources/bin/tool", root.join("tool")).unwrap();
        let manifest = parse_manifest(&format!(
            "{}{}",
            manifest_line(b"tool", "resources/bin/tool"),
            manifest_line(b"tool", "tool")
        ))
        .unwrap();
        assert!(verify(root.path(), &manifest, Method::Package)
            .unwrap()
            .is_empty());

        // 符号链接指向目录外时即使内容相同也要报告.
        fs::remove_file(root.join("tool")).unwrap();
        symlink("/etc/hosts", root.join("tool")).unwrap();
        let changed = verify(root.path(), &manifest, Method::Package).unwrap();
        assert_eq!(changed, vec!["tool".to_string()]);

        // AppImage 运行时追加的文件不算未登记文件.
        root.write("AppRun", b"run");
        root.write("usr/share/applications/oddity.desktop", b"desktop");
        root.write("resources/apparmor-profile", b"profile");
        assert!(verify(root.path(), &manifest, Method::AppImage)
            .unwrap()
            .iter()
            .all(|path| path == "tool"));
        let package_changed = verify(root.path(), &manifest, Method::Package).unwrap();
        assert!(package_changed.contains(&"AppRun".to_string()));
        assert!(!package_changed.contains(&"resources/apparmor-profile".to_string()));
    }

    #[test]
    fn reports_are_truncated_but_keep_the_total() {
        let root = TempDir::new("integrity-truncate");
        let mut text = String::new();
        for index in 0..(MAX_CHANGED_FILES + 2) {
            text.push_str(&manifest_line(b"missing", &format!("files/{index}")));
        }
        let manifest = parse_manifest(&text).unwrap();
        let (state, source, changed, total) =
            classify(root.path(), &manifest, Source::Bundled, Method::Package);
        assert_eq!(state, State::Changed);
        assert_eq!(source, Some(Source::Bundled));
        assert_eq!(changed.len(), MAX_CHANGED_FILES);
        assert_eq!(total, MAX_CHANGED_FILES + 2);
    }

    #[test]
    fn package_online_check_uses_the_release_manifest() {
        let root = TempDir::new("integrity-online");
        root.write("app.bin", b"content");
        let server = HttpServer::start();
        server.route(
            "/release/FILES.sha256",
            manifest_line(b"content", "app.bin"),
        );
        let status = check_at(
            &package_installation(&root),
            "1.0.0",
            &server.url("/release"),
            Duration::from_secs(2),
        );
        assert_eq!(status.state, State::Verified);
        assert_eq!(status.source, Some(Source::Online));
        assert!(status.checked_at > 0);
        assert_eq!(
            status.release_url,
            "https://github.com/LaT-SKY/Oddity/releases/tag/oddity-v1.0.0"
        );

        server.route("/release/FILES.sha256", manifest_line(b"other", "app.bin"));
        let status = check_at(
            &package_installation(&root),
            "1.0.0",
            &server.url("/release"),
            Duration::from_secs(2),
        );
        assert_eq!(status.state, State::Changed);
        assert_eq!(status.source, Some(Source::Online));
        assert_eq!(status.changed, vec!["app.bin".to_string()]);
        assert_eq!(status.changed_total, 1);
    }

    #[test]
    fn package_offline_check_uses_the_bundled_manifest() {
        let root = TempDir::new("integrity-offline");
        root.write("app.bin", b"content");
        root.write("FILES.sha256", manifest_line(b"content", "app.bin"));
        let server = HttpServer::start();
        let status = check_at(
            &package_installation(&root),
            "1.0.0",
            &server.url("/missing"),
            Duration::from_secs(2),
        );
        assert_eq!(status.state, State::Baseline);
        assert_eq!(status.source, Some(Source::Bundled));

        root.write("app.bin", b"tampered");
        let status = check_at(
            &package_installation(&root),
            "1.0.0",
            &server.url("/missing"),
            Duration::from_secs(2),
        );
        assert_eq!(status.state, State::Changed);
        assert_eq!(status.source, Some(Source::Bundled));

        fs::remove_file(root.join("FILES.sha256")).unwrap();
        let status = check_at(
            &package_installation(&root),
            "1.0.0",
            &server.url("/missing"),
            Duration::from_secs(2),
        );
        assert_eq!(status.state, State::Unverified);
        assert!(status.source.is_none());
    }

    #[test]
    fn appimage_online_check_hashes_the_whole_file() {
        let root = TempDir::new("integrity-appimage");
        let image = root.join("oddity-1.0.0-x86_64.AppImage");
        fs::write(&image, b"whole image").unwrap();
        let server = HttpServer::start();
        server.route(
            "/release/SHA256SUMS",
            format!(
                "{}  oddity-1.0.0-x86_64.AppImage\n",
                sha256_bytes(b"whole image")
            ),
        );
        let installation = Installation {
            method: Method::AppImage,
            root: root.path().to_path_buf(),
            image: Some(image.clone()),
        };
        let status = check_at(
            &installation,
            "1.0.0",
            &server.url("/release"),
            Duration::from_secs(2),
        );
        assert_eq!(status.state, State::Verified);
        assert_eq!(status.source, Some(Source::Online));

        // 用户重命名文件后仍能按唯一的 AppImage 资产核对.
        let renamed = root.join("renamed.AppImage");
        fs::rename(&image, &renamed).unwrap();
        let installation = Installation {
            method: Method::AppImage,
            root: root.path().to_path_buf(),
            image: Some(renamed),
        };
        assert_eq!(
            check_at(
                &installation,
                "1.0.0",
                &server.url("/release"),
                Duration::from_secs(2)
            )
            .state,
            State::Verified
        );

        server.route(
            "/release/SHA256SUMS",
            format!("{}  oddity-1.0.0-x86_64.AppImage\n", "a".repeat(64)),
        );
        let status = check_at(
            &installation,
            "1.0.0",
            &server.url("/release"),
            Duration::from_secs(2),
        );
        assert_eq!(status.state, State::Changed);
        assert_eq!(status.changed_total, 1);
    }

    #[test]
    fn appimage_offline_check_falls_back_to_the_bundled_manifest() {
        let root = TempDir::new("integrity-appimage-offline");
        let image_dir = TempDir::new("integrity-appimage-offline-image");
        let image = image_dir.join("oddity.AppImage");
        fs::write(&image, b"image").unwrap();
        root.write("app.bin", b"content");
        root.write("FILES.sha256", manifest_line(b"content", "app.bin"));
        let server = HttpServer::start();
        let installation = Installation {
            method: Method::AppImage,
            root: root.path().to_path_buf(),
            image: Some(image),
        };
        assert_eq!(
            check_at(
                &installation,
                "1.0.0",
                &server.url("/missing"),
                Duration::from_secs(2)
            )
            .state,
            State::Baseline
        );

        fs::remove_file(root.join("FILES.sha256")).unwrap();
        let status = check_at(
            &installation,
            "1.0.0",
            &server.url("/missing"),
            Duration::from_secs(2),
        );
        assert_eq!(status.state, State::Unverified);
    }

    #[test]
    fn appimage_checks_cover_missing_images_and_ambiguous_assets() {
        let root = TempDir::new("integrity-appimage-edge");
        root.write("FILES.sha256", manifest_line(b"content", "app.bin"));
        root.write("app.bin", b"content");
        let installation = Installation {
            method: Method::AppImage,
            root: root.path().to_path_buf(),
            image: None,
        };
        let status = check_at(
            &installation,
            "1.0.0",
            "http://127.0.0.1:1",
            Duration::from_millis(20),
        );
        assert_eq!(status.state, State::Unverified);

        let image_dir = TempDir::new("integrity-appimage-edge-image");
        let image = image_dir.join("oddity.AppImage");
        fs::write(&image, b"image").unwrap();
        let server = HttpServer::start();
        server.route(
            "/release/SHA256SUMS",
            format!(
                "{}  one.AppImage\n{}  two.AppImage\n",
                "a".repeat(64),
                "b".repeat(64)
            ),
        );
        let installation = Installation {
            method: Method::AppImage,
            root: root.path().to_path_buf(),
            image: Some(image),
        };
        let status = check_at(
            &installation,
            "1.0.0",
            &server.url("/release"),
            Duration::from_secs(2),
        );
        // 无法确定对应资产时回退到内置清单.
        assert_eq!(status.state, State::Baseline);
    }

    #[test]
    fn unsupported_installations_are_not_reported_as_verified() {
        let installation = Installation::from_executable(
            Some(Path::new(
                "/nix/store/abc/lib/oddity/resources/bin/oddity-backend",
            )),
            None,
        );
        assert_eq!(installation.method, Method::Nix);
        let status = check_at(
            &installation,
            "1.0.0",
            "http://127.0.0.1:1",
            Duration::from_millis(20),
        );
        assert_eq!(status.state, State::Unsupported);

        let dev = Installation::from_executable(
            Some(Path::new("/repo/launcher/target/debug/oddity-backend")),
            None,
        );
        assert_eq!(dev.method, Method::Unsupported);
    }

    #[test]
    fn detects_packaged_layouts_and_appimage_runs() {
        let root = TempDir::new("integrity-layout");
        root.write("resources/bin/oddity-backend", b"backend");
        let executable = root.join("resources/bin/oddity-backend");
        let installation = Installation::from_executable(Some(&executable), None);
        assert_eq!(installation.method, Method::Package);
        assert_eq!(installation.root, root.path());

        let installation =
            Installation::from_executable(Some(&executable), Some(root.join("oddity.AppImage")));
        assert_eq!(installation.method, Method::AppImage);
        assert_eq!(installation.root, root.path());
    }

    #[test]
    fn history_tracks_notices_per_app_version() {
        let dir = TempDir::new("integrity-history");
        let path = dir.join("integrity.json");
        let mut history = History::load(&path);
        assert!(history.state.is_none());
        assert!(history.save(&path).is_ok());

        let success = Status {
            state: State::Verified,
            source: Some(Source::Online),
            checked_at: 10,
            changed: Vec::new(),
            changed_total: 0,
            notice: None,
            app_version: "1.0.0".into(),
            release_url: String::new(),
        };
        assert_eq!(history.observe(&success), Some(Notice::Success));
        history.save(&path).unwrap();
        assert_eq!(History::load(&path).pending_notice, Some(Notice::Success));
        assert!(history.acknowledge(Notice::Success));
        assert!(!history.acknowledge(Notice::Success));
        history.save(&path).unwrap();

        let changed = Status {
            state: State::Changed,
            source: Some(Source::Online),
            checked_at: 20,
            changed: vec!["app.bin".into()],
            changed_total: 1,
            notice: None,
            app_version: "1.0.0".into(),
            release_url: String::new(),
        };
        assert_eq!(history.observe(&changed), Some(Notice::Changed));
        assert_eq!(history.observe(&changed), Some(Notice::Changed));
        assert!(history.acknowledge(Notice::Changed));
        assert_eq!(history.observe(&changed), None);
        assert_eq!(history.changed_notice_version.as_deref(), Some("1.0.0"));

        let next_version = Status {
            app_version: "1.0.1".into(),
            ..changed.clone()
        };
        assert_eq!(history.observe(&next_version), Some(Notice::Changed));
        assert_eq!(history.observe(&success), Some(Notice::Success));
        assert!(history.acknowledge(Notice::Success));

        let unverified = Status {
            state: State::Unverified,
            source: None,
            ..success.clone()
        };
        assert_eq!(history.observe(&unverified), None);
        assert!(history.pending_notice.is_none());
        assert_eq!(history.checked_at, unverified.checked_at);

        // 离线基线通过后恢复在线校验不算从失败恢复, 不再提示成功.
        let baseline = Status {
            state: State::Baseline,
            source: Some(Source::Bundled),
            ..success.clone()
        };
        assert_eq!(history.observe(&baseline), None);
        assert_eq!(history.observe(&success), None);

        // 断网时内置清单不一致只记录状态, 不弹重装引导.
        let offline_changed = Status {
            state: State::Changed,
            source: Some(Source::Bundled),
            app_version: "2.0.0".into(),
            ..baseline.clone()
        };
        assert_eq!(history.observe(&offline_changed), None);
        assert!(history.pending_notice.is_none());
    }

    #[test]
    fn history_keeps_unacknowledged_changes_until_confirmed() {
        let dir = TempDir::new("integrity-history-pending");
        let path = dir.join("integrity.json");
        let changed = Status {
            state: State::Changed,
            source: Some(Source::Online),
            checked_at: 10,
            changed: vec!["app.bin".into()],
            changed_total: 1,
            notice: None,
            app_version: "1.0.0".into(),
            release_url: String::new(),
        };
        let baseline = Status {
            state: State::Baseline,
            source: Some(Source::Bundled),
            checked_at: 20,
            ..changed.clone()
        };
        let mut history = History::load(&path);
        assert_eq!(history.observe(&changed), Some(Notice::Changed));
        assert_eq!(history.observe(&baseline), Some(Notice::Changed));
        assert_eq!(history.pending_changed, vec!["app.bin"]);
        assert_eq!(history.pending_changed_total, 1);
        assert_eq!(history.pending_source, Some(Source::Online));
        history.save(&path).unwrap();
        assert_eq!(
            History::load(&path).pending_changed,
            vec!["app.bin".to_string()]
        );

        // 应用版本变化后, 旧版本的未确认提示不再保留.
        let next_version = Status {
            app_version: "1.1.0".into(),
            ..baseline.clone()
        };
        assert_eq!(history.observe(&next_version), None);
        assert!(history.pending_changed.is_empty());

        assert_eq!(history.observe(&changed), Some(Notice::Changed));
        assert!(history.acknowledge(Notice::Changed));
        assert!(history.pending_changed.is_empty());
        assert!(history.pending_source.is_none());
        assert_eq!(history.observe(&baseline), None);
    }

    #[test]
    fn history_survives_missing_or_broken_files() {
        let dir = TempDir::new("integrity-history-broken");
        assert_eq!(
            History::load(&dir.join("missing.json")).pending_notice,
            None
        );
        dir.write("broken.json", b"{not json");
        assert_eq!(History::load(&dir.join("broken.json")).state, None);
    }
}
