use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use md5::Digest as _;

use crate::game::check::{
    join_install_path, PatchRef, UpdateItem, UpdateKind, UpdatePlan, PATCH_DISK_RESERVE,
};
use crate::game::config::GameUpdateConfig;
use crate::game::manifest::{Block, PakEntry, ResEntry};
use crate::game::plan::StageTimings;
use crate::http::HttpClient;
use crate::throttle::RateLimiter;
use crate::update::md5_file_cancellable;
use crate::{Error, Result};

#[derive(Debug, Clone)]
pub struct ApplyOptions {
    pub dry_run: bool,
    pub only: Option<String>,
    pub force: bool,
    pub update_ledger: bool,
    /// 下载前是否允许给 CDN 测速; 计费网络等策略由调用方关闭.
    pub probe_cdns: bool,
}

impl Default for ApplyOptions {
    fn default() -> Self {
        Self {
            dry_run: false,
            only: None,
            force: false,
            update_ledger: true,
            probe_cdns: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ApplyResult {
    pub path: String,
    pub bytes: u64,
    pub ok: bool,
    pub error: Option<String>,
    pub skipped: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ApplyReport {
    pub results: Vec<ApplyResult>,
    pub downloaded_bytes: u64,
    /// 各阶段实测耗时, 供吞吐估计和完成时间决策使用.
    pub timings: StageTimings,
    pub failed: usize,
    pub skipped: usize,
    pub ledger_error: Option<String>,
    pub cancelled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyStage {
    Download,
    Patch,
    Verify,
    Install,
}

impl ApplyStage {
    pub fn as_str(self) -> &'static str {
        match self {
            ApplyStage::Download => "download",
            ApplyStage::Patch => "patch",
            ApplyStage::Verify => "verify",
            ApplyStage::Install => "install",
        }
    }
}

const NETWORK_BUFFER: usize = 1024 * 1024;
#[cfg(not(test))]
const RANGE_THRESHOLD: u64 = 128 * 1024 * 1024;
#[cfg(test)]
const RANGE_THRESHOLD: u64 = 1;
#[cfg(not(test))]
const RANGE_CHUNK: u64 = 64 * 1024 * 1024;
#[cfg(test)]
const RANGE_CHUNK: u64 = 64 * 1024;
#[cfg(not(test))]
const RANGE_RETRIES: usize = 2;
#[cfg(test)]
const RANGE_RETRIES: usize = 1;
const SMALL_TARGET_HASH: u64 = 20 * 1024 * 1024;
/// 更新前测速的采样上限; 只用来选路, 不计入下载进度.
#[cfg(not(test))]
const CDN_PROBE_BYTES: u64 = 2 * 1024 * 1024;
#[cfg(test)]
const CDN_PROBE_BYTES: u64 = 64 * 1024;
/// 单个 CDN 的测速时间上限.
const CDN_PROBE_TIMEOUT: Duration = Duration::from_secs(4);
/// 计划下载量太小时, 测速的流量和延迟不划算, 直接按配置顺序下载.
#[cfg(not(test))]
const CDN_PROBE_MIN_BYTES: u64 = 32 * 1024 * 1024;
#[cfg(test)]
const CDN_PROBE_MIN_BYTES: u64 = 64 * 1024;

enum WorkerEvent {
    Started {
        index: usize,
    },
    Bytes {
        index: usize,
        bytes: u64,
    },
    Stage {
        index: usize,
        stage: ApplyStage,
    },
    Finished {
        result: ApplyResult,
        cancelled: bool,
        timings: StageTimings,
    },
}

/// 任务级下载活跃墙钟.
///
/// 多个 worker 和分块线程可能同时读网络; 这里统计所有读取区间的并集,
/// 用「总网络字节 / 活跃墙钟」换算出的才是聚合吞吐, 与默认估计口径一致。
struct DownloadClock {
    state: Mutex<DownloadClockState>,
}

#[derive(Default)]
struct DownloadClockState {
    active: usize,
    started: Option<Instant>,
    accumulated: Duration,
}

impl DownloadClock {
    fn new() -> Self {
        Self {
            state: Mutex::new(DownloadClockState::default()),
        }
    }

    /// 标记一段网络读取开始; 返回的守卫在 drop 时结束该段.
    fn begin(&self) -> DownloadGuard<'_> {
        let mut state = self.lock();
        if state.active == 0 {
            state.started = Some(Instant::now());
        }
        state.active += 1;
        DownloadGuard { clock: self }
    }

    /// 各读取区间并集的总纳秒数; 进行中的区间按当前时刻折算.
    fn wall_nanos(&self) -> u64 {
        let state = self.lock();
        let mut total = state.accumulated;
        if let Some(started) = state.started {
            total += started.elapsed();
        }
        u64::try_from(total.as_nanos()).unwrap_or(u64::MAX)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, DownloadClockState> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }
}

struct DownloadGuard<'a> {
    clock: &'a DownloadClock,
}

impl Drop for DownloadGuard<'_> {
    fn drop(&mut self) {
        let mut state = self.clock.lock();
        state.active -= 1;
        if state.active == 0 {
            if let Some(started) = state.started.take() {
                state.accumulated += started.elapsed();
            }
        }
    }
}

/// 等待应用许可时检查取消信号的间隔.
const APPLY_WAIT_SLICE: Duration = Duration::from_millis(50);

/// 补丁链的并发与暂存预算. 许可保留到产物安装或清理完成.
struct ApplyGate {
    state: Mutex<ApplyGateState>,
    released: Condvar,
    workspace_budget: u64,
}

#[derive(Default)]
struct ApplyGateState {
    limit: usize,
    active: usize,
    reserved: u64,
}

impl ApplyGate {
    fn new(limit: usize, workspace_budget: u64) -> Self {
        Self {
            state: Mutex::new(ApplyGateState {
                limit: limit.max(1),
                active: 0,
                reserved: 0,
            }),
            released: Condvar::new(),
            workspace_budget,
        }
    }

    /// 在同一把锁下预留空间, 避免多条链重复使用同一份余量.
    /// 取消回调在锁外执行, 空间查询与准入共用门锁.
    fn acquire_cancellable(
        &self,
        workspace: u64,
        available: &dyn Fn() -> Result<u64>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<ApplyPermit<'_>> {
        loop {
            if cancelled() {
                return Err(Error::Cancelled);
            }
            let mut state = self.lock();
            let available = available()?;
            let fits = state
                .reserved
                .checked_add(workspace)
                .filter(|bytes| *bytes <= self.workspace_budget)
                .and_then(|bytes| bytes.checked_add(PATCH_DISK_RESERVE))
                .is_some_and(|needed| available >= needed);
            if state.active < state.limit && fits {
                state.active += 1;
                state.reserved += workspace;
                return Ok(ApplyPermit {
                    gate: self,
                    workspace,
                });
            }
            if state.active == 0 {
                return Err(Error::Integrity("暂存空间不足, 无法应用补丁链".into()));
            }
            let (state, _) = self
                .released
                .wait_timeout(state, APPLY_WAIT_SLICE)
                .unwrap_or_else(|error| error.into_inner());
            drop(state);
        }
    }

    #[cfg(test)]
    fn acquire(&self) -> ApplyPermit<'_> {
        self.acquire_cancellable(0, &|| Ok(u64::MAX), &|| false)
            .expect("acquire test permit")
    }

    /// 当前并发数; 仅测试用来确认阶段边界.
    #[cfg(test)]
    fn active(&self) -> usize {
        self.lock().active
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ApplyGateState> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }
}

struct ApplyPermit<'a> {
    gate: &'a ApplyGate,
    workspace: u64,
}

impl Drop for ApplyPermit<'_> {
    fn drop(&mut self) {
        let mut state = self.gate.lock();
        state.active -= 1;
        state.reserved -= self.workspace;
        self.gate.released.notify_all();
    }
}

/// 应用并发上限: 留一半核心给下载和校验, 最多 4 路.
fn apply_limit_for(parallelism: usize) -> usize {
    (parallelism / 2).clamp(1, 4)
}

fn default_apply_limit() -> usize {
    let parallelism = std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(1);
    apply_limit_for(parallelism)
}

struct Progress<'a> {
    tx: &'a Sender<WorkerEvent>,
    index: usize,
    /// 同一路径在整份计划里的序号, 不受 --only 选择影响.
    tag_index: usize,
    cancelled: &'a (dyn Fn() -> bool + Send + Sync),
    limiter: &'a RateLimiter,
    download_clock: &'a DownloadClock,
    bytes: AtomicU64,
    // 各阶段的真实字节与耗时; 分块线程共用同一份, 用原子量累计.
    download_bytes: AtomicU64,
    apply_bytes: AtomicU64,
    apply_nanos: AtomicU64,
    verify_bytes: AtomicU64,
    verify_nanos: AtomicU64,
    install_bytes: AtomicU64,
    install_nanos: AtomicU64,
}

impl Progress<'_> {
    fn new<'a>(
        tx: &'a Sender<WorkerEvent>,
        index: usize,
        tag_index: usize,
        cancelled: &'a (dyn Fn() -> bool + Send + Sync),
        limiter: &'a RateLimiter,
        download_clock: &'a DownloadClock,
    ) -> Progress<'a> {
        Progress {
            tx,
            index,
            tag_index,
            cancelled,
            limiter,
            download_clock,
            bytes: AtomicU64::new(0),
            download_bytes: AtomicU64::new(0),
            apply_bytes: AtomicU64::new(0),
            apply_nanos: AtomicU64::new(0),
            verify_bytes: AtomicU64::new(0),
            verify_nanos: AtomicU64::new(0),
            install_bytes: AtomicU64::new(0),
            install_nanos: AtomicU64::new(0),
        }
    }

    /// 为本次网络读取扣减限速令牌, 取消时终止当前文件.
    fn throttle(&self, bytes: u64) -> Result<()> {
        if self.limiter.acquire(bytes, self.cancelled) {
            Ok(())
        } else {
            Err(Error::Cancelled)
        }
    }

    fn add(&self, bytes: u64) {
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
        let _ = self.tx.send(WorkerEvent::Bytes {
            index: self.index,
            bytes,
        });
    }

    fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    fn stage(&self, stage: ApplyStage) {
        let _ = self.tx.send(WorkerEvent::Stage {
            index: self.index,
            stage,
        });
    }

    fn cancelled(&self) -> bool {
        (self.cancelled)()
    }

    /// 标记一段真实网络读取; 返回的守卫在离开作用域时结束计时.
    fn begin_download(&self) -> DownloadGuard<'_> {
        self.download_clock.begin()
    }

    /// 网络耗时由任务级 DownloadClock 统一统计, 这里只累计真实字节.
    fn add_download(&self, bytes: u64) {
        self.download_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    fn add_apply(&self, bytes: u64, elapsed: Duration) {
        record_stage(&self.apply_bytes, &self.apply_nanos, bytes, elapsed);
    }

    fn add_verify(&self, bytes: u64, elapsed: Duration) {
        record_stage(&self.verify_bytes, &self.verify_nanos, bytes, elapsed);
    }

    fn add_install(&self, bytes: u64, elapsed: Duration) {
        record_stage(&self.install_bytes, &self.install_nanos, bytes, elapsed);
    }

    fn timings(&self) -> StageTimings {
        StageTimings {
            download_bytes: self.download_bytes.load(Ordering::Relaxed),
            // 下载耗时由任务级 DownloadClock 在汇总时覆盖, 单项汇总先留空.
            download_nanos: 0,
            apply_bytes: self.apply_bytes.load(Ordering::Relaxed),
            apply_nanos: self.apply_nanos.load(Ordering::Relaxed),
            verify_bytes: self.verify_bytes.load(Ordering::Relaxed),
            verify_nanos: self.verify_nanos.load(Ordering::Relaxed),
            install_bytes: self.install_bytes.load(Ordering::Relaxed),
            install_nanos: self.install_nanos.load(Ordering::Relaxed),
        }
    }
}

/// 累计一个阶段的字节与耗时; 纳秒饱和到 u64, 避免极端时钟溢出.
fn record_stage(bytes: &AtomicU64, nanos: &AtomicU64, amount: u64, elapsed: Duration) {
    bytes.fetch_add(amount, Ordering::Relaxed);
    nanos.fetch_add(
        u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX),
        Ordering::Relaxed,
    );
}

enum Prepared {
    AlreadyCorrect,
    Full,
    File(PathBuf),
    Patches(Vec<PathBuf>),
}

pub fn apply_plan(
    config: &GameUpdateConfig,
    plan: &UpdatePlan,
    opts: &ApplyOptions,
) -> ApplyReport {
    apply_plan_observed(config, plan, opts, &mut |_| {}, &mut |_, _| {}, &|| false)
}

pub fn apply_plan_observed(
    config: &GameUpdateConfig,
    plan: &UpdatePlan,
    opts: &ApplyOptions,
    on_file: &mut dyn FnMut(&UpdateItem),
    on_bytes: &mut dyn FnMut(&str, u64),
    cancelled: &(dyn Fn() -> bool + Send + Sync),
) -> ApplyReport {
    apply_plan_observed_full(
        config,
        plan,
        opts,
        0,
        on_file,
        on_bytes,
        &mut |_, _| {},
        &mut |_| {},
        cancelled,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn apply_plan_observed_with_stage(
    config: &GameUpdateConfig,
    plan: &UpdatePlan,
    opts: &ApplyOptions,
    on_file: &mut dyn FnMut(&UpdateItem),
    on_bytes: &mut dyn FnMut(&str, u64),
    on_stage: &mut dyn FnMut(&str, ApplyStage),
    cancelled: &(dyn Fn() -> bool + Send + Sync),
) -> ApplyReport {
    apply_plan_observed_full(
        config,
        plan,
        opts,
        0,
        on_file,
        on_bytes,
        on_stage,
        &mut |_| {},
        cancelled,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn apply_plan_observed_full(
    config: &GameUpdateConfig,
    plan: &UpdatePlan,
    opts: &ApplyOptions,
    limit_bytes_per_second: u64,
    on_file: &mut dyn FnMut(&UpdateItem),
    on_bytes: &mut dyn FnMut(&str, u64),
    on_stage: &mut dyn FnMut(&str, ApplyStage),
    on_result: &mut dyn FnMut(&ApplyResult),
    cancelled: &(dyn Fn() -> bool + Send + Sync),
) -> ApplyReport {
    let staging = config.patcher_dir.join("tmp").join("nte-native");
    let selected: Vec<&UpdateItem> = plan
        .items
        .iter()
        .filter(|item| match &opts.only {
            Some(filter) => item.path.contains(filter),
            None => true,
        })
        .collect();

    let mut report = ApplyReport::default();
    // 并发下载的耗时按任务级活跃墙钟统计, 与默认估计的聚合吞吐口径一致.
    let download_clock = DownloadClock::new();
    if !plan.unverified.is_empty() {
        report.failed = plan.unverified.len();
        report.results = plan
            .unverified
            .iter()
            .map(|item| ApplyResult {
                path: item.path.clone(),
                bytes: 0,
                ok: false,
                error: Some("资源尚未完整校验, 请先执行完整校验".into()),
                skipped: false,
            })
            .collect();
        return finish_timings(report, &download_clock);
    }
    if !opts.dry_run {
        let storage_paths = [
            config.patcher_dir.join("ResList.xml"),
            staging.join("Res"),
            staging.join("Patch"),
            staging.join("Pak"),
        ];
        for path in storage_paths {
            let valid = path
                .strip_prefix(&config.install_root)
                .map_err(|_| Error::Integrity("资源暂存目录超出安装目录".into()))
                .and_then(|relative| {
                    crate::paths::ensure_no_symlink(&config.install_root, relative)
                });
            if let Err(error) = valid {
                report.failed = 1;
                report.ledger_error = Some(error.to_string());
                return finish_timings(report, &download_clock);
            }
        }
    }
    if selected.is_empty() {
        report.cancelled = cancelled();
        if opts.update_ledger && !opts.dry_run && !report.cancelled && !plan.verified.is_empty() {
            let verified = plan
                .verified
                .iter()
                .map(|item| item.path().to_string())
                .collect::<Vec<_>>();
            if let Err(error) = crate::game::ledger::update_after_apply(
                config,
                plan,
                &verified,
                plan.items.is_empty(),
            ) {
                report.ledger_error = Some(error.to_string());
            }
        }
        return finish_timings(report, &download_clock);
    }

    if opts.dry_run {
        for item in &selected {
            on_file(item);
            report.results.push(ApplyResult {
                path: item.path.clone(),
                bytes: 0,
                ok: true,
                error: None,
                skipped: true,
            });
            report.skipped += 1;
        }
        return finish_timings(report, &download_clock);
    }

    let tag_indices = selected_tag_indices(plan, &selected);
    cleanup_stale_parts(&staging, plan);
    let max_threads = if plan.max_threads == 0 {
        config.max_threads
    } else {
        plan.max_threads
    }
    .clamp(1, 8);
    let workers = max_threads.min(selected.len()).max(1);
    let allow_ranges = max_threads > 1;
    let range_threads = if selected.len() == 1 { max_threads } else { 1 };
    let http = HttpClient::new();
    let mut worker_config = config.clone();
    // 多个文件已经用满连接数时，每个大文件只开一条分块连接；只有一个
    // 大文件时才把整组连接都用于它的分块下载。
    worker_config.max_threads = range_threads;
    // 大任务开始前给 CDN 做一次小样本测速; 失败或条件不满足时沿用配置顺序.
    let download_bytes: u64 = selected.iter().map(|item| item.download_size).sum();
    if opts.probe_cdns
        && cdn_probe_is_worthwhile(
            worker_config.cdns.len(),
            download_bytes,
            limit_bytes_per_second,
        )
    {
        if let Some(order) = rank_cdns(&worker_config, &selected, &http, cancelled) {
            worker_config.cdns = order;
        }
    }
    let worker_config = &worker_config;
    let next = AtomicUsize::new(0);
    let install_lock = Mutex::new(());
    // 下载量和安装后的正向净增长单独留足, 剩余空间才可分配给并发链产物.
    let patch_growth = selected.iter().fold(0u64, |total, item| {
        total.saturating_add(item.patch_disk_growth())
    });
    let workspace_budget = if selected.iter().any(|item| item.has_patch()) {
        match crate::disk::available_bytes(&staging) {
            Ok(available) => available
                .saturating_sub(download_bytes)
                .saturating_sub(patch_growth)
                .saturating_sub(PATCH_DISK_RESERVE),
            Err(error) => {
                report.failed = 1;
                report.ledger_error = Some(error.to_string());
                return finish_timings(report, &download_clock);
            }
        }
    } else {
        0
    };
    let apply_gate = ApplyGate::new(default_apply_limit(), workspace_budget);
    let limiter = RateLimiter::new(limit_bytes_per_second);
    let (tx, rx) = mpsc::channel();
    let mut worker_txs: Vec<Sender<WorkerEvent>> = (0..workers).map(|_| tx.clone()).collect();
    drop(tx);
    let selected = &selected;
    let tag_indices = &tag_indices;
    let staging = &staging;
    let install_lock = &install_lock;
    let apply_gate = &apply_gate;
    let next = &next;
    let http = &http;
    let limiter = &limiter;
    let download_clock = &download_clock;
    let mut completed: Vec<String> = Vec::new();
    let mut report = std::thread::scope(|scope| {
        for tx in worker_txs.drain(..) {
            scope.spawn(move || {
                worker_loop(
                    worker_config,
                    plan,
                    opts,
                    selected,
                    tag_indices,
                    staging,
                    http,
                    install_lock,
                    apply_gate,
                    next,
                    &tx,
                    limiter,
                    download_clock,
                    cancelled,
                    allow_ranges,
                );
            });
        }
        let mut report = ApplyReport::default();
        for event in rx {
            match event {
                WorkerEvent::Started { index } => on_file(selected[index]),
                WorkerEvent::Bytes { index, bytes } => {
                    report.downloaded_bytes += bytes;
                    on_bytes(&selected[index].path, bytes);
                }
                WorkerEvent::Stage { index, stage } => {
                    on_stage(&selected[index].path, stage);
                }
                WorkerEvent::Finished {
                    result,
                    cancelled: was_cancelled,
                    timings,
                } => {
                    report.timings.add(&timings);
                    if was_cancelled {
                        report.cancelled = true;
                    } else if result.ok && !result.skipped {
                        completed.push(result.path.clone());
                        if opts.update_ledger {
                            if let Err(error) = crate::game::ledger::update_after_apply(
                                config, plan, &completed, false,
                            ) {
                                report.ledger_error = Some(error.to_string());
                            }
                        }
                    } else if !result.ok {
                        report.failed += 1;
                    }
                    on_result(&result);
                    report.results.push(result);
                }
            }
        }
        report
    });
    // 各 worker 只累计了下载字节, 这里统一写入任务级活跃墙钟.
    report.timings.download_nanos = download_clock.wall_nanos();

    report.results.sort_by(|a, b| a.path.cmp(&b.path));
    report.cancelled |= cancelled();
    completed.extend(plan.verified.iter().map(|item| item.path().to_string()));
    if opts.update_ledger && !completed.is_empty() {
        let bump = !report.cancelled && report.failed == 0;
        match crate::game::ledger::update_after_apply(config, plan, &completed, bump) {
            Ok(()) => report.ledger_error = None,
            Err(error) => report.ledger_error = Some(error.to_string()),
        }
    }
    report
}

/// 用任务级下载墙钟收尾报告; 没开始过任何下载时保持 0.
fn finish_timings(mut report: ApplyReport, download_clock: &DownloadClock) -> ApplyReport {
    report.timings.download_nanos = download_clock.wall_nanos();
    report
}

#[allow(clippy::too_many_arguments)]
fn worker_loop(
    config: &GameUpdateConfig,
    plan: &UpdatePlan,
    opts: &ApplyOptions,
    selected: &[&UpdateItem],
    tag_indices: &[usize],
    staging: &Path,
    http: &HttpClient,
    install_lock: &Mutex<()>,
    apply_gate: &ApplyGate,
    next: &AtomicUsize,
    tx: &Sender<WorkerEvent>,
    limiter: &RateLimiter,
    download_clock: &DownloadClock,
    cancelled: &(dyn Fn() -> bool + Send + Sync),
    allow_ranges: bool,
) {
    loop {
        if cancelled() {
            return;
        }
        let index = next.fetch_add(1, Ordering::Relaxed);
        if index >= selected.len() {
            return;
        }
        let progress = Progress::new(
            tx,
            index,
            tag_indices[index],
            cancelled,
            limiter,
            download_clock,
        );
        let _ = tx.send(WorkerEvent::Started { index });
        let (result, was_cancelled, timings) = process_item(
            config,
            plan,
            opts,
            selected[index],
            staging,
            http,
            install_lock,
            apply_gate,
            &progress,
            allow_ranges,
        );
        let _ = tx.send(WorkerEvent::Finished {
            result,
            cancelled: was_cancelled,
            timings,
        });
        if was_cancelled {
            return;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn process_item(
    config: &GameUpdateConfig,
    _plan: &UpdatePlan,
    opts: &ApplyOptions,
    item: &UpdateItem,
    staging: &Path,
    http: &HttpClient,
    install_lock: &Mutex<()>,
    apply_gate: &ApplyGate,
    progress: &Progress<'_>,
    allow_ranges: bool,
) -> (ApplyResult, bool, StageTimings) {
    let target = join_install_path(&config.install_root, &item.path);
    let outcome = run_item(
        config,
        opts,
        item,
        &target,
        staging,
        http,
        install_lock,
        apply_gate,
        progress,
        allow_ranges,
    );
    let (ok, cancelled, error) = match outcome {
        Ok(()) => (true, false, None),
        Err(Error::Cancelled) => (false, true, None),
        Err(error) => (false, false, Some(error.to_string())),
    };
    (
        ApplyResult {
            path: item.path.clone(),
            bytes: progress.bytes(),
            ok,
            error,
            skipped: false,
        },
        cancelled,
        progress.timings(),
    )
}

#[allow(clippy::too_many_arguments)]
fn run_item(
    config: &GameUpdateConfig,
    opts: &ApplyOptions,
    item: &UpdateItem,
    target: &Path,
    staging: &Path,
    http: &HttpClient,
    install_lock: &Mutex<()>,
    apply_gate: &ApplyGate,
    progress: &Progress<'_>,
    allow_ranges: bool,
) -> Result<()> {
    let relative = crate::paths::relative_path(&item.path)?;
    let check_paths = || -> Result<()> {
        crate::paths::ensure_no_symlink(&config.install_root, &relative)?;
        let staging_relative = staging
            .strip_prefix(&config.install_root)
            .map_err(|_| Error::Integrity("资源暂存目录超出安装目录".into()))?;
        crate::paths::ensure_no_symlink(&config.install_root, staging_relative)
    };
    check_paths()?;
    let prepared = prepare_item(
        config,
        opts,
        item,
        target,
        staging,
        http,
        progress,
        allow_ranges,
    )?;
    let mut permit = None;
    let temp = match prepared {
        Prepared::AlreadyCorrect => return Ok(()),
        Prepared::File(temp) => Some(temp),
        Prepared::Full => None,
        Prepared::Patches(patches) => {
            let applied = (|| -> Result<PathBuf> {
                // 校验和等待安装时产物仍在暂存盘, 不能提前归还预算.
                permit = Some(apply_gate.acquire_cancellable(
                    item.patch_workspace(),
                    &|| crate::disk::available_bytes(staging),
                    &|| progress.cancelled(),
                )?);
                if progress.cancelled() {
                    return Err(Error::Cancelled);
                }
                progress.stage(ApplyStage::Patch);
                check_paths()?;
                let final_path = apply_patch_chain(item, target, staging, &patches, progress)?;
                progress.stage(ApplyStage::Verify);
                if !verify_observed(progress, &final_path, item.size, &item.md5)? {
                    return Err(Error::Integrity(format!(
                        "patched output mismatch for {}",
                        target.display()
                    )));
                }
                Ok(final_path)
            })();
            match applied {
                Ok(temp) => Some(temp),
                // 保留已生成的最终产物供下次续传.
                Err(Error::Cancelled) => return Err(Error::Cancelled),
                Err(_) => {
                    remove_chain_temps(staging, item, progress.tag_index);
                    permit.take();
                    None
                }
            }
        }
    };
    let temp = match temp {
        Some(temp) => temp,
        None => {
            // 回退整包同样需要暂存预算, 不能在补丁失败后绕过空间限制.
            if item.has_patch() {
                permit = Some(apply_gate.acquire_cancellable(
                    item.size,
                    &|| crate::disk::available_bytes(staging),
                    &|| progress.cancelled(),
                )?);
            }
            download_full(config, item, staging, http, progress, allow_ranges)?
        }
    };
    progress.stage(ApplyStage::Install);
    let installed = with_install_lock(install_lock, || {
        if progress.cancelled() {
            return Err(Error::Cancelled);
        }
        check_paths()?;
        install_observed(progress, &temp, target, item.size)
    });
    if installed.is_ok() {
        remove_patch_temps(staging, item, progress.tag_index);
        remove_chain_temps(staging, item, progress.tag_index);
    }
    drop(permit);
    installed
}

fn with_install_lock<T>(lock: &Mutex<()>, action: impl FnOnce() -> Result<T>) -> Result<T> {
    let guard = lock
        .lock()
        .map_err(|_| Error::Unsupported("安装锁已损坏".into()))?;
    let result = action();
    drop(guard);
    result
}

#[allow(clippy::too_many_arguments)]
fn prepare_item(
    config: &GameUpdateConfig,
    opts: &ApplyOptions,
    item: &UpdateItem,
    target: &Path,
    staging: &Path,
    http: &HttpClient,
    progress: &Progress<'_>,
    allow_ranges: bool,
) -> Result<Prepared> {
    if !opts.force && quick_target_matches(item, target) {
        return Ok(Prepared::AlreadyCorrect);
    }

    if !item.patches.is_empty() && target.is_file() {
        if let Some(final_temp) = chain_final_temp(staging, item, progress.tag_index) {
            if final_temp.is_file() && verify_observed(progress, &final_temp, item.size, &item.md5)?
            {
                // 完整补丁产物是之前下载并应用补丁的结果, 计入本轮已下载量.
                progress.add(item.download_size);
                return Ok(Prepared::File(final_temp));
            }
        }
        remove_chain_temps(staging, item, progress.tag_index);
        let mut paths = Vec::with_capacity(item.patches.len());
        let mut failed = None;
        for patch in &item.patches {
            match download_patch(config, item, patch, staging, http, progress, allow_ranges) {
                Ok(path) => paths.push(path),
                Err(Error::Cancelled) => return Err(Error::Cancelled),
                Err(error) => {
                    failed = Some(error);
                    break;
                }
            }
        }
        if failed.is_none() {
            return Ok(Prepared::Patches(paths));
        }
        // 补丁下载失败时还可以退回整文件；继续走完整下载。
    }

    Ok(Prepared::Full)
}

fn quick_target_matches(item: &UpdateItem, target: &Path) -> bool {
    if item.size == 0 || item.size > SMALL_TARGET_HASH {
        return false;
    }
    let Ok(metadata) = fs::metadata(target) else {
        return false;
    };
    metadata.len() == item.size && verify(target, item.size, &item.md5).unwrap_or(false)
}

fn download_full(
    config: &GameUpdateConfig,
    item: &UpdateItem,
    staging: &Path,
    http: &HttpClient,
    progress: &Progress<'_>,
    allow_ranges: bool,
) -> Result<PathBuf> {
    progress.stage(ApplyStage::Download);
    let tag = item_tag(item, progress.tag_index);
    let temp = match &item.kind {
        UpdateKind::Res { res } => {
            download_res(config, res, &tag, staging, http, progress, allow_ranges)?
        }
        UpdateKind::PakEntry {
            pak_md5,
            pak_size,
            entry,
        } => download_pak_entry(
            config, pak_md5, *pak_size, entry, &tag, staging, http, progress,
        )?,
    };
    Ok(temp)
}

fn download_patch(
    config: &GameUpdateConfig,
    item: &UpdateItem,
    patch: &PatchRef,
    staging: &Path,
    http: &HttpClient,
    progress: &Progress<'_>,
    allow_ranges: bool,
) -> Result<PathBuf> {
    progress.stage(ApplyStage::Download);
    let url = res_url(config, &patch.md5, patch.size)?;
    let temp = patch_temp_path(staging, item, patch, progress.tag_index);
    download_to_temp(
        config,
        http,
        &url,
        &temp,
        patch.size,
        &patch.md5,
        &[],
        progress,
        allow_ranges,
    )?;
    progress.stage(ApplyStage::Verify);
    Ok(temp)
}

fn download_res(
    config: &GameUpdateConfig,
    res: &ResEntry,
    tag: &str,
    staging: &Path,
    http: &HttpClient,
    progress: &Progress<'_>,
    allow_ranges: bool,
) -> Result<PathBuf> {
    let url = res_url(config, &res.md5, res.filesize)?;
    let temp = staging.join("Res").join(format!(
        "{}.{}.{}.part",
        res.md5.to_ascii_lowercase(),
        res.filesize,
        tag
    ));
    download_to_temp(
        config,
        http,
        &url,
        &temp,
        res.filesize,
        &res.md5,
        &res.blocks,
        progress,
        allow_ranges,
    )?;
    progress.stage(ApplyStage::Verify);
    Ok(temp)
}

#[allow(clippy::too_many_arguments)]
fn download_pak_entry(
    config: &GameUpdateConfig,
    pak_md5: &str,
    pak_size: u64,
    entry: &PakEntry,
    tag: &str,
    staging: &Path,
    http: &HttpClient,
    progress: &Progress<'_>,
) -> Result<PathBuf> {
    if entry.md5.len() != 32 || entry.offset.checked_add(entry.size).is_none() {
        return Err(Error::Integrity(format!(
            "bad PatcherPak entry for {}",
            entry.name
        )));
    }
    let url = res_url(config, pak_md5, pak_size)?;
    let temp = staging
        .join("Pak")
        .join(format!("{}.{}.{}.part", entry.md5, entry.size, tag));
    download_range_to_temp(
        config,
        http,
        &url,
        &temp,
        entry.offset,
        entry.size,
        &entry.md5,
        progress,
    )?;
    progress.stage(ApplyStage::Verify);
    Ok(temp)
}

fn apply_patch_chain(
    item: &UpdateItem,
    target: &Path,
    staging: &Path,
    patch_paths: &[PathBuf],
    progress: &Progress<'_>,
) -> Result<PathBuf> {
    if patch_paths.len() != item.patches.len() {
        return Err(Error::Integrity("补丁链长度与计划不一致".into()));
    }
    let mut current = target.to_path_buf();
    let mut current_is_temp = false;
    for (step, patch_path) in patch_paths.iter().enumerate() {
        if progress.cancelled() {
            if current_is_temp {
                fs::remove_file(&current).ok();
            }
            return Err(Error::Cancelled);
        }
        let patch = &item.patches[step];
        let current_size = fs::metadata(&current)?.len();
        if current_size != patch.old_size {
            return Err(Error::Integrity(format!(
                "patch step {} expects {} bytes, found {}",
                step, patch.old_size, current_size
            )));
        }
        let out = chain_temp(staging, item, step, progress.tag_index);
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent)?;
        }
        if out.exists() {
            fs::remove_file(&out)?;
        }
        let started = Instant::now();
        crate::game::patch::apply_hdiff(&current, patch_path, &out)?;
        progress.add_apply(patch.new_size, started.elapsed());
        if current_is_temp {
            fs::remove_file(&current).ok();
        }
        current = out;
        current_is_temp = true;
    }
    Ok(current)
}

fn chain_temp(staging: &Path, item: &UpdateItem, step: usize, index: usize) -> PathBuf {
    let patch = &item.patches[step];
    staging.join("Patch").join(format!(
        "{}.{}.{}.{}.tmp",
        item_tag(item, index),
        step,
        patch.new_md5,
        patch.new_size
    ))
}

fn chain_final_temp(staging: &Path, item: &UpdateItem, index: usize) -> Option<PathBuf> {
    item.patches
        .len()
        .checked_sub(1)
        .map(|step| chain_temp(staging, item, step, index))
}

fn remove_chain_temps(staging: &Path, item: &UpdateItem, index: usize) {
    for step in 0..item.patches.len() {
        fs::remove_file(chain_temp(staging, item, step, index)).ok();
    }
}

fn patch_temp_path(staging: &Path, item: &UpdateItem, patch: &PatchRef, index: usize) -> PathBuf {
    staging.join("Patch").join(format!(
        "{}.{}.{}.part",
        patch.md5,
        patch.size,
        item_tag(item, index)
    ))
}

fn remove_patch_temps(staging: &Path, item: &UpdateItem, index: usize) {
    for patch in &item.patches {
        fs::remove_file(patch_temp_path(staging, item, patch, index)).ok();
    }
}

#[allow(clippy::too_many_arguments)]
fn download_to_temp(
    config: &GameUpdateConfig,
    http: &HttpClient,
    url: &str,
    temp: &Path,
    expected_size: u64,
    expected_md5: &str,
    blocks: &[Block],
    progress: &Progress<'_>,
    allow_ranges: bool,
) -> Result<u64> {
    let relative = temp
        .strip_prefix(&config.install_root)
        .map_err(|_| Error::Integrity("资源暂存目录超出安装目录".into()))?;
    crate::paths::ensure_no_symlink(&config.install_root, relative)?;
    let index = range_index_path(temp);
    crate::paths::ensure_no_symlink(
        &config.install_root,
        index
            .strip_prefix(&config.install_root)
            .map_err(|_| Error::Integrity("资源暂存目录超出安装目录".into()))?,
    )?;
    if let Some(parent) = temp.parent() {
        fs::create_dir_all(parent)?;
    }
    if temp.is_file() {
        let length = fs::metadata(temp)?.len();
        if length == expected_size {
            if verify_observed(progress, temp, expected_size, expected_md5)? {
                fs::remove_file(range_index_path(temp)).ok();
                progress.add(length);
                return Ok(0);
            }
            // 分块下载的稀疏文件长度已经等于目标大小，但块索引还在；
            // 保留它让下面按块续传，不能当完整文件删掉。
            if !(allow_ranges && range_index_path(temp).is_file()) {
                fs::remove_file(temp)?;
            }
        } else if length > expected_size {
            fs::remove_file(temp)?;
        }
    }

    if allow_ranges && expected_size >= RANGE_THRESHOLD {
        let index = range_index_path(temp);
        match download_ranges(
            config,
            http,
            url,
            temp,
            &index,
            expected_size,
            expected_md5,
            blocks,
            progress,
        ) {
            Ok(bytes) => return Ok(bytes),
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            Err(Error::Unsupported(_)) | Err(Error::Integrity(_)) => {
                // Range 不可用或分块校验失败时重新走顺序下载，避免留下
                // 预分配的半成品文件。
                fs::remove_file(temp).ok();
                fs::remove_file(&index).ok();
            }
            // 网络/磁盘错误保留块索引，下一次任务可以从已完成块继续。
            Err(error) => return Err(error),
        }
    }

    // 本轮开始前已存在的有效前缀会继续使用, 完成后与本次下载量一起计入进度.
    let mut resumed = temp.metadata().map(|meta| meta.len()).unwrap_or(0);
    let mut last_error = None;
    for (i, cdn) in config.cdns.iter().enumerate() {
        let attempt_url = request_url(config, cdn, i, url);
        for attempt in 0..=RANGE_RETRIES {
            if progress.cancelled() {
                return Err(Error::Cancelled);
            }
            let existing = temp.metadata().map(|meta| meta.len()).unwrap_or(0);
            match fetch_to_file(http, &attempt_url, temp, existing, expected_size, progress) {
                Ok(bytes) => match verify_observed(progress, temp, expected_size, expected_md5) {
                    Ok(true) => {
                        // 服务端忽略 Range 时会从头覆盖文件, 此时旧前缀不再计入.
                        if resumed > 0 && resumed.saturating_add(bytes) <= expected_size {
                            progress.add(resumed);
                        }
                        return Ok(bytes);
                    }
                    Ok(false) => {
                        fs::remove_file(temp).ok();
                        resumed = 0;
                        last_error = Some(Error::Integrity(format!(
                            "download md5 mismatch for {}",
                            temp.display()
                        )));
                        break;
                    }
                    Err(error) => {
                        last_error = Some(error);
                        break;
                    }
                },
                Err(Error::Cancelled) => return Err(Error::Cancelled),
                Err(error) => {
                    last_error = Some(error);
                    if attempt < RANGE_RETRIES {
                        std::thread::sleep(Duration::from_millis(250));
                    }
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(|| Error::Http(format!("download failed: {url}"))))
}

#[allow(clippy::too_many_arguments)]
fn download_range_to_temp(
    config: &GameUpdateConfig,
    http: &HttpClient,
    url: &str,
    temp: &Path,
    offset: u64,
    size: u64,
    expected_md5: &str,
    progress: &Progress<'_>,
) -> Result<u64> {
    let relative = temp
        .strip_prefix(&config.install_root)
        .map_err(|_| Error::Integrity("资源暂存目录超出安装目录".into()))?;
    crate::paths::ensure_no_symlink(&config.install_root, relative)?;
    let index = range_index_path(temp);
    crate::paths::ensure_no_symlink(
        &config.install_root,
        index
            .strip_prefix(&config.install_root)
            .map_err(|_| Error::Integrity("资源暂存目录超出安装目录".into()))?,
    )?;
    if let Some(parent) = temp.parent() {
        fs::create_dir_all(parent)?;
    }
    if temp.is_file() {
        let length = fs::metadata(temp)?.len();
        if length == size {
            if verify_observed(progress, temp, size, expected_md5)? {
                return Ok(0);
            }
            fs::remove_file(temp)?;
        } else if length > size {
            fs::remove_file(temp)?;
        }
    }
    let end = offset
        .checked_add(size)
        .and_then(|value| value.checked_sub(1))
        .ok_or_else(|| Error::Integrity("PatcherPak range overflow".into()))?;
    let mut last_error = None;
    for (i, cdn) in config.cdns.iter().enumerate() {
        let attempt_url = request_url(config, cdn, i, url);
        for attempt in 0..=RANGE_RETRIES {
            if progress.cancelled() {
                return Err(Error::Cancelled);
            }
            let existing = temp.metadata().map(|meta| meta.len()).unwrap_or(0);
            let range = format!("bytes={}-{}", offset + existing, end);
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(temp)?;
            match fetch_range_to_file(
                http,
                &attempt_url,
                &file,
                existing,
                &range,
                size.saturating_sub(existing),
                progress,
            ) {
                Ok(bytes) => match verify_observed(progress, temp, size, expected_md5) {
                    Ok(true) => return Ok(bytes),
                    Ok(false) => {
                        fs::remove_file(temp).ok();
                        last_error = Some(Error::Integrity(format!(
                            "PatcherPak entry md5 mismatch for {}",
                            temp.display()
                        )));
                        break;
                    }
                    Err(error) => {
                        last_error = Some(error);
                        break;
                    }
                },
                Err(Error::Cancelled) => return Err(Error::Cancelled),
                Err(error @ Error::Unsupported(_)) => {
                    last_error = Some(error);
                    break;
                }
                Err(error) => {
                    last_error = Some(error);
                    if attempt < RANGE_RETRIES {
                        std::thread::sleep(Duration::from_millis(250));
                    }
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(|| Error::Http("PatcherPak range download failed".into())))
}

#[allow(clippy::too_many_arguments)]
fn download_ranges(
    config: &GameUpdateConfig,
    http: &HttpClient,
    url: &str,
    temp: &Path,
    index_path: &Path,
    expected_size: u64,
    expected_md5: &str,
    blocks: &[Block],
    progress: &Progress<'_>,
) -> Result<u64> {
    let chunks = range_chunks(expected_size, blocks);
    if chunks.is_empty() {
        return Ok(0);
    }

    let resume = read_range_index(index_path, expected_size, chunks.len());
    let mut done = match resume {
        Some(done) => done,
        None => {
            fs::remove_file(temp).ok();
            fs::remove_file(index_path).ok();
            Vec::new()
        }
    };
    if !temp.is_file() && !done.is_empty() {
        fs::remove_file(index_path).ok();
        return Err(Error::Integrity("块索引对应的临时文件已丢失".into()));
    }

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(temp)?;
    // 临时文件被截断时，旧索引里的块不再可信。
    if file.metadata()?.len() != expected_size {
        if index_path.exists() {
            done.clear();
            fs::remove_file(index_path).ok();
        }
        file.set_len(expected_size)?;
    }
    if !index_path.exists() {
        write_range_index_header(index_path, expected_size, chunks.len())?;
    }

    for chunk in &chunks {
        if done.contains(&chunk.index) {
            continue;
        }
        if progress.cancelled() {
            return Err(Error::Cancelled);
        }
    }

    let resumed_bytes = chunks
        .iter()
        .filter(|chunk| done.contains(&chunk.index))
        .map(|chunk| chunk.size)
        .sum::<u64>();
    let total = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let first_error: Mutex<Option<Error>> = Mutex::new(None);
    let done_lock = Mutex::new(done);
    let threads = config.max_threads.clamp(1, 8).min(chunks.len());
    let chunk_list = &chunks;
    let file_ref = &file;
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| loop {
                if failed.load(Ordering::Relaxed) || progress.cancelled() {
                    return;
                }
                let index = total.fetch_add(1, Ordering::Relaxed);
                if index >= chunk_list.len() {
                    return;
                }
                let chunk = &chunk_list[index];
                if done_lock
                    .lock()
                    .map(|done| done.contains(&chunk.index))
                    .unwrap_or(true)
                {
                    continue;
                }
                match fetch_chunk(config, http, url, file_ref, chunk, progress) {
                    Ok(_) => {
                        if let Some(expected) = &chunk.md5 {
                            if !md5_range(file_ref, chunk.start, chunk.size)
                                .is_ok_and(|got| got.eq_ignore_ascii_case(expected))
                            {
                                failed.store(true, Ordering::Relaxed);
                                set_first_error(
                                    &first_error,
                                    Error::Integrity(format!("chunk {} md5 mismatch", chunk.index)),
                                );
                                return;
                            }
                        }
                        if let Ok(mut done) = done_lock.lock() {
                            done.push(chunk.index);
                        }
                        if let Err(error) = append_range_index(index_path, chunk.index) {
                            failed.store(true, Ordering::Relaxed);
                            set_first_error(&first_error, error);
                            return;
                        }
                    }
                    Err(Error::Cancelled) => {
                        failed.store(true, Ordering::Relaxed);
                        set_first_error(&first_error, Error::Cancelled);
                        return;
                    }
                    Err(error) => {
                        failed.store(true, Ordering::Relaxed);
                        set_first_error(&first_error, error);
                        return;
                    }
                }
            });
        }
    });

    if let Some(error) = first_error.into_inner().unwrap_or(None) {
        return Err(error);
    }
    if progress.cancelled() {
        return Err(Error::Cancelled);
    }
    if !verify_observed(progress, temp, expected_size, expected_md5)? {
        fs::remove_file(temp).ok();
        fs::remove_file(index_path).ok();
        return Err(Error::Integrity(format!(
            "range download md5 mismatch for {}",
            temp.display()
        )));
    }
    if resumed_bytes > 0 {
        progress.add(resumed_bytes);
    }
    fs::remove_file(index_path).ok();
    Ok(0)
}

fn set_first_error(slot: &Mutex<Option<Error>>, error: Error) {
    if let Ok(mut slot) = slot.lock() {
        if slot.is_none() {
            *slot = Some(error);
        }
    }
}

#[derive(Debug)]
struct RangeChunk {
    index: u64,
    start: u64,
    size: u64,
    md5: Option<String>,
}

fn range_chunks(size: u64, blocks: &[Block]) -> Vec<RangeChunk> {
    let usable_blocks = !blocks.is_empty()
        && blocks.iter().all(|block| block.size > 0)
        && blocks.iter().enumerate().all(|(index, block)| {
            index == 0
                || blocks[index - 1].start.checked_add(blocks[index - 1].size) == Some(block.start)
        })
        && blocks.first().is_some_and(|block| block.start == 0)
        && blocks
            .last()
            .is_some_and(|block| block.start.checked_add(block.size) == Some(size));
    if usable_blocks {
        return blocks
            .iter()
            .enumerate()
            .map(|(index, block)| RangeChunk {
                index: index as u64,
                start: block.start,
                size: block.size,
                md5: (!block.md5.is_empty()).then(|| block.md5.to_ascii_lowercase()),
            })
            .collect();
    }
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut index = 0;
    while start < size {
        let length = (size - start).min(RANGE_CHUNK);
        chunks.push(RangeChunk {
            index,
            start,
            size: length,
            md5: None,
        });
        start += length;
        index += 1;
    }
    chunks
}

fn fetch_chunk(
    config: &GameUpdateConfig,
    http: &HttpClient,
    url: &str,
    file: &File,
    chunk: &RangeChunk,
    progress: &Progress<'_>,
) -> Result<u64> {
    let mut last_error = None;
    for (i, cdn) in config.cdns.iter().enumerate() {
        let attempt_url = request_url(config, cdn, i, url);
        for attempt in 0..=RANGE_RETRIES {
            if progress.cancelled() {
                return Err(Error::Cancelled);
            }
            let end = chunk.start + chunk.size - 1;
            let range = format!("bytes={}-{}", chunk.start, end);
            match fetch_range_to_file(
                http,
                &attempt_url,
                file,
                chunk.start,
                &range,
                chunk.size,
                progress,
            ) {
                Ok(bytes) => return Ok(bytes),
                Err(Error::Cancelled) => return Err(Error::Cancelled),
                Err(error @ Error::Unsupported(_)) => {
                    last_error = Some(error);
                    break;
                }
                Err(error) => {
                    last_error = Some(error);
                    if attempt < RANGE_RETRIES {
                        std::thread::sleep(Duration::from_millis(250));
                    }
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(|| Error::Http("range download failed".into())))
}

fn fetch_to_file(
    http: &HttpClient,
    url: &str,
    temp: &Path,
    existing: u64,
    expected_size: u64,
    progress: &Progress<'_>,
) -> Result<u64> {
    let mut request = http.request(
        url,
        transfer_timeout(expected_size.saturating_sub(existing)),
    );
    if existing > 0 {
        request = request.header("Range", format!("bytes={existing}-"));
    }
    let mut response = match request.call() {
        Ok(response) => response,
        Err(ureq::Error::StatusCode(416)) if existing > 0 => return Ok(0),
        Err(error) => return Err(Error::Http(format!("GET {url}: {error}"))),
    };
    let status = response.status().as_u16();
    let append = status == 206 && existing > 0;
    let mut output = if append {
        OpenOptions::new().append(true).open(temp)?
    } else {
        File::create(temp)?
    };
    let mut reader = response.body_mut().as_reader();
    let mut buffer = vec![0u8; NETWORK_BUFFER];
    let mut total = 0u64;
    // 只统计本次真正从网络读取的字节; 续传前缀由调用方另行计入进度.
    let _download = progress.begin_download();
    loop {
        if progress.cancelled() {
            output.flush()?;
            return Err(Error::Cancelled);
        }
        let read = reader
            .read(&mut buffer)
            .map_err(|error| Error::Http(format!("read {url}: {error}")))?;
        if read == 0 {
            break;
        }
        progress.throttle(read as u64)?;
        output.write_all(&buffer[..read])?;
        total += read as u64;
        progress.add(read as u64);
    }
    output.flush()?;
    if status != 200 && status != 206 {
        return Err(Error::Http(format!("GET {url}: HTTP {status}")));
    }
    progress.add_download(total);
    Ok(total)
}

fn fetch_range_to_file(
    http: &HttpClient,
    url: &str,
    file: &File,
    write_offset: u64,
    range: &str,
    range_size: u64,
    progress: &Progress<'_>,
) -> Result<u64> {
    let mut response = http
        .request(url, transfer_timeout(range_size))
        .header("Range", range)
        .call()
        .map_err(|error| Error::Http(format!("GET {url}: {error}")))?;
    let status = response.status().as_u16();
    if status != 206 {
        return Err(Error::Unsupported(format!(
            "server ignored Range for {url}: HTTP {status}"
        )));
    }
    let mut reader = response.body_mut().as_reader();
    let mut buffer = vec![0u8; NETWORK_BUFFER];
    let mut written = 0u64;
    // 读取区间进入任务级活跃墙钟; 并发块的重叠时间只算一次.
    let _download = progress.begin_download();
    loop {
        if progress.cancelled() {
            return Err(Error::Cancelled);
        }
        let read = reader
            .read(&mut buffer)
            .map_err(|error| Error::Http(format!("read {url}: {error}")))?;
        if read == 0 {
            break;
        }
        progress.throttle(read as u64)?;
        write_all_at(file, &buffer[..read], write_offset + written)?;
        written += read as u64;
        progress.add(read as u64);
    }
    progress.add_download(written);
    Ok(written)
}

fn write_all_at(file: &File, mut bytes: &[u8], mut offset: u64) -> Result<()> {
    use std::os::unix::fs::FileExt;
    while !bytes.is_empty() {
        let written = file.write_at(bytes, offset)?;
        if written == 0 {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "write_at returned 0",
            )));
        }
        bytes = &bytes[written..];
        offset += written as u64;
    }
    Ok(())
}

fn range_index_path(temp: &Path) -> PathBuf {
    temp.with_extension("cb.idx")
}

fn read_range_index(path: &Path, size: u64, chunks: usize) -> Option<Vec<u64>> {
    let text = fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    if lines.next()? != "ODDITY-CB1" {
        return None;
    }
    let size_line = lines.next()?;
    if size_line != format!("size={size}") {
        return None;
    }
    let chunks_line = lines.next()?;
    if chunks_line != format!("chunks={chunks}") {
        return None;
    }
    let mut done = Vec::new();
    for line in lines {
        if let Ok(value) = line.trim().parse::<u64>() {
            if value < chunks as u64 && !done.contains(&value) {
                done.push(value);
            }
        }
    }
    Some(done)
}

fn write_range_index_header(path: &Path, size: u64, chunks: usize) -> Result<()> {
    let text = format!("ODDITY-CB1\nsize={size}\nchunks={chunks}\n");
    let temporary = path.with_extension("cb.idx.tmp");
    fs::write(&temporary, text)?;
    fs::rename(&temporary, path)?;
    Ok(())
}

fn append_range_index(path: &Path, index: u64) -> Result<()> {
    let mut file = OpenOptions::new().append(true).create(true).open(path)?;
    writeln!(file, "{index}")?;
    file.flush()?;
    Ok(())
}

fn md5_range(file: &File, start: u64, size: u64) -> Result<String> {
    use std::os::unix::fs::FileExt;
    let mut hasher = md5::Md5::new();
    let mut buffer = vec![0u8; NETWORK_BUFFER];
    let mut offset = start;
    let mut remaining = size;
    while remaining > 0 {
        let want = (remaining as usize).min(buffer.len());
        let read = file.read_at(&mut buffer[..want], offset)?;
        if read == 0 {
            return Err(Error::Integrity("块读取提前结束".into()));
        }
        hasher.update(&buffer[..read]);
        offset += read as u64;
        remaining -= read as u64;
    }
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        value.push_str(&format!("{byte:02x}"));
    }
    value
}

fn transfer_timeout(bytes: u64) -> Duration {
    // 以 256 KiB/s 的最慢速度估算，避免大文件在正常慢速链路上被提前中断。
    let seconds = (bytes / (256 * 1024)).clamp(120, 6 * 60 * 60);
    Duration::from_secs(seconds)
}

fn request_url(config: &GameUpdateConfig, cdn: &str, index: usize, original: &str) -> String {
    if index == 0 {
        return original.to_string();
    }
    let suffix = original
        .split_once("/Res/")
        .map(|(_, value)| value)
        .unwrap_or("");
    format!(
        "{}/{}/Res/{}",
        cdn.trim_end_matches('/'),
        config.branch,
        suffix
    )
}

pub fn res_url(config: &GameUpdateConfig, md5: &str, size: u64) -> Result<String> {
    let md5 = md5.to_ascii_lowercase();
    let first = md5.chars().next().unwrap_or('0');
    let cdn = config
        .cdns
        .first()
        .map(String::as_str)
        .ok_or_else(|| Error::Parse("缺少资源 CDN 配置".into()))?;
    Ok(format!(
        "{}/{}/Res/{}/{}.{}",
        cdn.trim_end_matches('/'),
        config.branch,
        first,
        md5,
        size
    ))
}

/// 测速只在大任务、多节点且未限速时进行; 计费网络等外部条件由调用方排除.
fn cdn_probe_is_worthwhile(
    cdn_count: usize,
    download_bytes: u64,
    limit_bytes_per_second: u64,
) -> bool {
    cdn_count > 1 && download_bytes >= CDN_PROBE_MIN_BYTES && limit_bytes_per_second == 0
}

/// 从计划里挑最大的文件做采样, 返回它的原始 URL 和可用大小.
fn cdn_probe_target(config: &GameUpdateConfig, items: &[&UpdateItem]) -> Option<(String, u64)> {
    items
        .iter()
        .filter_map(|item| match &item.kind {
            UpdateKind::Res { res } => res_url(config, &res.md5, res.filesize)
                .ok()
                .map(|url| (url, res.filesize)),
            UpdateKind::PakEntry {
                pak_md5, pak_size, ..
            } => res_url(config, pak_md5, *pak_size)
                .ok()
                .map(|url| (url, *pak_size)),
        })
        .filter(|(_, size)| *size > 0)
        .max_by_key(|(_, size)| *size)
}

/// 按实测吞吐把 CDN 从快到慢排序; 没有任何样本时返回 None, 保持配置顺序.
fn rank_cdns(
    config: &GameUpdateConfig,
    items: &[&UpdateItem],
    http: &HttpClient,
    cancelled: &(dyn Fn() -> bool + Send + Sync),
) -> Option<Vec<String>> {
    let (url, size) = cdn_probe_target(config, items)?;
    let want = size.min(CDN_PROBE_BYTES);
    let mut rates: Vec<(usize, u64)> = Vec::with_capacity(config.cdns.len());
    for (index, cdn) in config.cdns.iter().enumerate() {
        if cancelled() {
            return None;
        }
        let probe_url = request_url(config, cdn, index, &url);
        rates.push((index, probe_rate(http, &probe_url, want)));
    }
    if rates.iter().all(|(_, rate)| *rate == 0) {
        return None;
    }
    // 稳定排序: 同样快的节点保持配置顺序, 测速失败的排到最后.
    rates.sort_by_key(|left| std::cmp::Reverse(left.1));
    Some(
        rates
            .into_iter()
            .map(|(index, _)| config.cdns[index].clone())
            .collect(),
    )
}

/// 读取一小段完整样本并返回吞吐 (字节/秒); 请求失败、读取中断或没取满时按 0 处理.
fn probe_rate(http: &HttpClient, url: &str, want: u64) -> u64 {
    let started = Instant::now();
    let mut response = match http
        .request(url, CDN_PROBE_TIMEOUT)
        .header("Range", format!("bytes=0-{}", want - 1))
        .call()
    {
        Ok(response) => response,
        Err(_) => return 0,
    };
    let mut reader = response.body_mut().as_reader();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut received = 0u64;
    while received < want {
        if started.elapsed() >= CDN_PROBE_TIMEOUT {
            break;
        }
        let remaining = (want - received).min(buffer.len() as u64) as usize;
        match reader.read(&mut buffer[..remaining]) {
            Ok(0) | Err(_) => break,
            Ok(read) => received += read as u64,
        }
    }
    // 提前结束、读取失败或超时都会让样本不完整, 不能拿这一小段算吞吐.
    if received < want {
        return 0;
    }
    let elapsed = started.elapsed().as_nanos().max(1);
    ((received as u128 * 1_000_000_000) / elapsed).min(u64::MAX as u128) as u64
}

fn verify(path: &Path, expected_size: u64, expected_md5: &str) -> Result<bool> {
    verify_cancellable(path, expected_size, expected_md5, &|| false)
}

fn verify_cancellable(
    path: &Path,
    expected_size: u64,
    expected_md5: &str,
    cancelled: &dyn Fn() -> bool,
) -> Result<bool> {
    let metadata = fs::metadata(path)?;
    if metadata.len() != expected_size {
        return Ok(false);
    }
    let got = md5_file_cancellable(path, cancelled)?;
    Ok(got.eq_ignore_ascii_case(expected_md5))
}

/// 带进度上下文的校验; 只有完整通过才计入耗时和字节, 避免把提前
/// 返回的尺寸不符或取消算成校验吞吐.
fn verify_observed(
    progress: &Progress<'_>,
    path: &Path,
    expected_size: u64,
    expected_md5: &str,
) -> Result<bool> {
    let started = Instant::now();
    let ok = verify_cancellable(path, expected_size, expected_md5, &|| progress.cancelled())?;
    if ok {
        progress.add_verify(expected_size, started.elapsed());
    }
    Ok(ok)
}

fn install_temp(temp: &Path, target: &Path, cancelled: &dyn Fn() -> bool) -> Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut adjacent = crate::paths::TemporaryFile::new(target)?;
    if cancelled() {
        return Err(Error::Cancelled);
    }
    match fs::rename(temp, &adjacent.path) {
        Ok(()) => {
            adjacent.file = File::open(&adjacent.path)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::CrossesDevices => {
            let mut output = &adjacent.file;
            crate::update::copy_cancellable(&mut File::open(temp)?, &mut output, cancelled)?;
        }
        Err(error) => return Err(error.into()),
    }
    adjacent.commit(target, cancelled)?;
    fs::remove_file(temp).ok();
    Ok(())
}

/// 安装阶段只在成功后记录耗时, 失败或取消的尝试不计入吞吐样本.
fn install_observed(progress: &Progress<'_>, temp: &Path, target: &Path, size: u64) -> Result<()> {
    let started = Instant::now();
    install_temp(temp, target, &|| progress.cancelled())?;
    progress.add_install(size, started.elapsed());
    Ok(())
}

fn path_tag(path: &str) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in path.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

/// 每个 item 的唯一暂存标识.
///
/// 清单允许重复路径, 这里用「同一路径在整份计划里的序号」区分; 该序号与
/// --only 选择无关, 因此中断后换一种选择仍能续传。同一路径的 item 增删时
/// 序号会变化, 旧暂存会被清理。
fn item_tag(item: &UpdateItem, index: usize) -> String {
    item_tag_for(&item.path, index)
}

/// 计算 selected 中每个 item 的暂存序号(同一路径在 plan.items 中的出现次序).
fn selected_tag_indices(plan: &UpdatePlan, selected: &[&UpdateItem]) -> Vec<usize> {
    let mut counts = HashMap::new();
    let mut chosen = selected.iter().peekable();
    let mut indices = Vec::with_capacity(selected.len());
    for item in &plan.items {
        let ordinal = counts.entry(&item.path).or_insert(0);
        if chosen.peek().is_some_and(|next| std::ptr::eq(item, **next)) {
            indices.push(*ordinal);
            chosen.next();
        }
        *ordinal += 1;
    }
    indices
}

fn item_tag_for(path: &str, index: usize) -> String {
    format!("{}-{index}", path_tag(path))
}

/// 保留整份计划的精确暂存路径, 清除过期下载、索引和链产物.
/// --only 不改变清理范围, 未选中项仍可在下一轮续传.
fn cleanup_stale_parts(staging: &Path, plan: &UpdatePlan) {
    let items: Vec<&UpdateItem> = plan.items.iter().collect();
    let tag_indices = selected_tag_indices(plan, &items);
    let mut keep = HashSet::new();
    for (item, index) in items.into_iter().zip(tag_indices) {
        let tag = item_tag(item, index);
        let full = match &item.kind {
            UpdateKind::Res { res } => staging.join("Res").join(format!(
                "{}.{}.{}.part",
                res.md5.to_ascii_lowercase(),
                res.filesize,
                tag
            )),
            UpdateKind::PakEntry { entry, .. } => staging
                .join("Pak")
                .join(format!("{}.{}.{}.part", entry.md5, entry.size, tag)),
        };
        keep.insert(range_index_path(&full));
        keep.insert(full);
        for (step, patch) in item.patches.iter().enumerate() {
            let temp = patch_temp_path(staging, item, patch, index);
            keep.insert(range_index_path(&temp));
            keep.insert(temp);
            keep.insert(chain_temp(staging, item, step, index));
        }
    }
    for directory in [
        staging.join("Res"),
        staging.join("Patch"),
        staging.join("Pak"),
    ] {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if (name.ends_with(".part") || name.ends_with(".cb.idx") || name.ends_with(".tmp"))
                && !keep.contains(&entry.path())
            {
                fs::remove_file(entry.path()).ok();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::check::{UpdateKind, UpdatePlan, UpdateReason};
    use crate::game::manifest::{PakEntry, ResEntry};
    use crate::testing::{HttpServer, TempDir};

    const BRANCH: &str = "publish_ob";

    fn config_for(dir: &TempDir) -> GameUpdateConfig {
        GameUpdateConfig {
            launcher_root: dir.join("install/NTELauncher"),
            install_root: dir.join("install"),
            config_dir: dir.join("install/NTELauncher/ResFilesM/1289/PatcherConfig"),
            patcher_dir: dir.join("install/NTELauncher/UserData/Patcher/PatcherSDK"),
            branch: BRANCH.to_string(),
            update_branch: "publish_Updata".to_string(),
            app_id: "1289".to_string(),
            cdns: vec!["https://cdn.test/clientRes".to_string()],
            rules: Vec::new(),
            check_dirs: Vec::new(),
            max_threads: 5,
        }
    }

    fn res_item(path: &str, md5: &str, size: u64) -> UpdateItem {
        UpdateItem {
            path: path.to_string(),
            size,
            md5: md5.to_string(),
            reason: UpdateReason::Missing,
            kind: UpdateKind::Res {
                res: ResEntry {
                    filename: path.to_string(),
                    filesize: size,
                    md5: md5.to_string(),
                    timestamp: None,
                    check: false,
                    blocks: Vec::new(),
                },
            },
            patches: Vec::new(),
            download_size: size,
        }
    }

    fn patch_item(
        path: &str,
        old_md5: &str,
        old_size: u64,
        new_md5: &str,
        new_size: u64,
        patches: &[(&str, u64)],
    ) -> UpdateItem {
        let mut item = res_item(path, new_md5, new_size);
        let mut old_md5 = old_md5.to_string();
        let mut old_size = old_size;
        for (md5, size) in patches {
            item.patches.push(PatchRef {
                md5: md5.to_string(),
                size: *size,
                old_md5: old_md5.clone(),
                old_size,
                new_md5: new_md5.to_string(),
                new_size,
            });
            old_md5 = new_md5.to_string();
            old_size = new_size;
        }
        item.download_size = item.patches.iter().map(|patch| patch.size).sum();
        item
    }

    fn pak_item(path: &str, entry_md5: &str, offset: u64, size: u64, pak_size: u64) -> UpdateItem {
        UpdateItem {
            path: path.to_string(),
            size,
            md5: entry_md5.to_string(),
            reason: UpdateReason::Missing,
            kind: UpdateKind::PakEntry {
                pak_md5: "aa".repeat(16),
                pak_size,
                entry: PakEntry {
                    name: path.to_string(),
                    start: 0,
                    offset,
                    size,
                    md5: entry_md5.to_string(),
                    check: false,
                },
            },
            patches: Vec::new(),
            download_size: size,
        }
    }

    fn plan_of(items: Vec<UpdateItem>) -> UpdatePlan {
        UpdatePlan {
            remote_version: "1.4.16".to_string(),
            remote_tag: "baseTag".to_string(),
            remote_hash: "HASH".to_string(),
            items,
            max_threads: 5,
            ..Default::default()
        }
    }

    fn res_path(md5: &str, size: u64) -> String {
        let md5 = md5.to_ascii_lowercase();
        format!(
            "/cdn/{BRANCH}/Res/{}/{md5}.{size}",
            md5.chars().next().unwrap_or('0')
        )
    }

    fn res_temp(
        config: &GameUpdateConfig,
        path: &str,
        md5: &str,
        size: u64,
        index: usize,
    ) -> PathBuf {
        config.patcher_dir.join("tmp/nte-native/Res").join(format!(
            "{}.{}.{}.part",
            md5,
            size,
            item_tag_for(path, index)
        ))
    }

    #[test]
    fn resource_install_rejects_symlinks_and_traversal_before_download() {
        let dir = TempDir::new("resource-path-boundary");
        let outside = TempDir::new("resource-outside");
        let config = config_for(&dir);
        fs::create_dir_all(&config.install_root).unwrap();
        std::os::unix::fs::symlink(outside.path(), config.install_root.join("data")).unwrap();
        outside.write("victim.bin", b"kept");
        for path in ["data/victim.bin", "../victim.bin"] {
            let plan = plan_of(vec![res_item(path, &crate::update::md5_bytes(b"good"), 4)]);
            let report = apply_plan(&config, &plan, &ApplyOptions::default());
            assert_eq!(report.failed, 1);
            assert_eq!(report.downloaded_bytes, 0);
            assert_eq!(outside.read("victim.bin"), b"kept");
        }
    }

    #[test]
    fn apply_refuses_unverified_plans_without_mutation() {
        let dir = TempDir::new("apply-unverified");
        let config = config_for(&dir);
        let mut plan = plan_of(Vec::new());
        plan.unverified.push(res_item("data/unknown.bin", "aa", 4));
        let report = apply_plan(&config, &plan, &ApplyOptions::default());
        assert_eq!(report.failed, 1);
        assert!(!report.results[0].ok);
        assert!(!config.patcher_dir.exists());
    }

    #[test]
    fn res_url_uses_the_first_cdn_and_a_lowercase_md5() {
        let dir = TempDir::new("dl-url");
        let mut config = config_for(&dir);
        config.cdns = vec![
            "https://cdn.test/clientRes/".to_string(),
            "https://backup.test/res".to_string(),
        ];
        assert_eq!(
            res_url(&config, &"AB".repeat(16), 12).unwrap(),
            format!(
                "https://cdn.test/clientRes/{BRANCH}/Res/a/{}.12",
                "ab".repeat(16)
            )
        );

        config.cdns.clear();
        assert!(res_url(&config, "", 0).is_err());
    }

    #[test]
    fn cancelling_before_workers_start_is_reported_without_writing_files() {
        let dir = TempDir::new("cancel-before-worker");
        let config = config_for(&dir);
        let plan = plan_of(vec![res_item("data/a.bin", &"ab".repeat(16), 5)]);
        let report = apply_plan_observed(
            &config,
            &plan,
            &ApplyOptions::default(),
            &mut |_| panic!("不能启动已取消的任务"),
            &mut |_, _| {},
            &|| true,
        );
        assert!(report.cancelled);
        assert!(report.results.is_empty());
        assert!(report.timings.is_empty());
        assert!(!config.install_root.join("data/a.bin").exists());
        assert!(!config.patcher_dir.join("ResList.xml").exists());
    }

    #[test]
    fn dry_run_reports_without_downloading() {
        let dir = TempDir::new("dl-dry");
        let config = config_for(&dir);
        let plan = plan_of(vec![
            res_item("data/a.bin", &"aa".repeat(16), 3),
            res_item("data/b.bin", &"bb".repeat(16), 4),
        ]);
        let stale = config.patcher_dir.join("tmp/nte-native/Res/stale.part");
        fs::create_dir_all(stale.parent().expect("parent")).expect("staging");
        fs::write(&stale, b"stale").expect("stale part");
        let report = apply_plan(
            &config,
            &plan,
            &ApplyOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        assert!(stale.is_file(), "dry-run 不应清理暂存文件");
        assert_eq!(report.results.len(), 2);
        assert!(report
            .results
            .iter()
            .all(|result| result.ok && result.skipped && result.bytes == 0));
        assert_eq!(report.skipped, 2);
        assert_eq!(report.downloaded_bytes, 0);
        assert_eq!(report.failed, 0);
        assert!(!dir.is_file("install/data/a.bin"));
    }

    #[test]
    fn apply_plan_downloads_res_files_and_updates_the_ledger() {
        let dir = TempDir::new("dl-apply");
        let server = HttpServer::start();
        let mut config = config_for(&dir);
        config.cdns = vec![server.url("/cdn")];
        let content = b"payload bytes";
        let md5 = crate::update::md5_bytes(content);
        server.route(res_path(&md5, content.len() as u64), content);

        let report = apply_plan(
            &config,
            &plan_of(vec![res_item("data/a.bin", &md5, content.len() as u64)]),
            &ApplyOptions::default(),
        );
        assert_eq!(report.failed, 0);
        assert!(report.results[0].ok);
        assert!(!report.results[0].skipped);
        assert_eq!(report.downloaded_bytes, content.len() as u64);
        assert_eq!(dir.read("install/data/a.bin"), content);
        // 整包路径只有下载、校验、安装三个阶段有真实耗时.
        assert!(report.timings.download_bytes > 0);
        assert!(report.timings.download_nanos > 0);
        assert!(report.timings.verify_bytes > 0);
        assert!(report.timings.verify_nanos > 0);
        assert!(report.timings.install_bytes > 0);
        assert!(report.timings.install_nanos > 0);
        assert_eq!(report.timings.apply_bytes, 0);
        assert_eq!(report.timings.apply_nanos, 0);

        let ledger = crate::game::check::load_local_ledger(&config)
            .expect("load ledger")
            .expect("ledger written");
        assert_eq!(ledger.version, "1.4.16");
        assert_eq!(ledger.tag, "baseTag");
        assert_eq!(ledger.res[0].filename, "data/a.bin");
        assert_eq!(ledger.res[0].md5, md5);
    }

    #[test]
    fn apply_plan_passes_the_bandwidth_limit_to_the_transfer() {
        let dir = TempDir::new("dl-limit");
        let server = HttpServer::start();
        let mut config = config_for(&dir);
        config.cdns = vec![server.url("/cdn")];
        let content = b"limited payload";
        let md5 = crate::update::md5_bytes(content);
        server.route(res_path(&md5, content.len() as u64), content);
        let plan = plan_of(vec![res_item(
            "data/limited.bin",
            &md5,
            content.len() as u64,
        )]);

        let report = apply_plan_observed_full(
            &config,
            &plan,
            &ApplyOptions::default(),
            4096,
            &mut |_| {},
            &mut |_, _| {},
            &mut |_, _| {},
            &mut |_| {},
            &|| false,
        );
        assert_eq!(report.failed, 0);
        assert_eq!(report.downloaded_bytes, content.len() as u64);
        assert_eq!(dir.read("install/data/limited.bin"), content);
    }

    #[test]
    fn apply_plan_skips_files_that_already_match() {
        let dir = TempDir::new("dl-skip");
        let server = HttpServer::start();
        let mut config = config_for(&dir);
        config.cdns = vec![server.url("/cdn")];
        let content = b"payload bytes";
        let md5 = crate::update::md5_bytes(content);
        let path = res_path(&md5, content.len() as u64);
        server.route(&path, content);
        dir.write("install/data/a.bin", content);

        let plan = plan_of(vec![res_item("data/a.bin", &md5, content.len() as u64)]);
        let report = apply_plan(&config, &plan, &ApplyOptions::default());
        assert_eq!(report.downloaded_bytes, 0);
        assert_eq!(report.results[0].bytes, 0);
        assert_eq!(server.hits(&path), 0);

        let report = apply_plan(
            &config,
            &plan,
            &ApplyOptions {
                force: true,
                ..Default::default()
            },
        );
        assert_eq!(report.downloaded_bytes, content.len() as u64);
        assert_eq!(server.hits(&path), 1);
    }

    #[test]
    fn apply_plan_records_failures_without_touching_the_ledger() {
        let dir = TempDir::new("dl-fail");
        let server = HttpServer::start();
        let mut config = config_for(&dir);
        config.cdns = vec![server.url("/cdn")];

        let report = apply_plan(
            &config,
            &plan_of(vec![res_item("data/a.bin", &"aa".repeat(16), 5)]),
            &ApplyOptions {
                update_ledger: false,
                ..Default::default()
            },
        );
        assert_eq!(report.failed, 1);
        assert!(!report.results[0].ok);
        assert!(report.results[0]
            .error
            .as_deref()
            .expect("error text")
            .contains("404"));
        // 请求在读取响应体前就失败, 不应产生任何阶段耗时.
        assert!(report.timings.is_empty());
        assert!(report.ledger_error.is_none());
        assert!(!config.patcher_dir.join("ResList.xml").exists());
    }

    #[test]
    fn apply_plan_reuses_a_verified_partial_download() {
        let dir = TempDir::new("dl-reuse");
        let server = HttpServer::start();
        let mut config = config_for(&dir);
        config.cdns = vec![server.url("/cdn")];
        let content = b"payload bytes";
        let md5 = crate::update::md5_bytes(content);
        let path = res_path(&md5, content.len() as u64);
        server.route(&path, content);

        let part = res_temp(&config, "data/a.bin", &md5, content.len() as u64, 0);
        fs::create_dir_all(part.parent().expect("parent")).expect("create staging");
        fs::write(&part, content).expect("write part");

        let report = apply_plan(
            &config,
            &plan_of(vec![res_item("data/a.bin", &md5, content.len() as u64)]),
            &ApplyOptions::default(),
        );
        // 已存在的完整临时文件计入本轮已下载量, 但不会再次请求 CDN.
        assert_eq!(report.downloaded_bytes, content.len() as u64);
        assert_eq!(server.hits(&path), 0);
        assert_eq!(dir.read("install/data/a.bin"), content);
        // 复用已有文件不产生下载耗时, 但仍要计入校验和安装.
        assert_eq!(report.timings.download_bytes, 0);
        assert_eq!(report.timings.download_nanos, 0);
        assert!(report.timings.verify_bytes > 0);
        assert!(report.timings.install_bytes > 0);
    }

    #[test]
    fn apply_plan_resumes_an_incomplete_download() {
        let dir = TempDir::new("dl-resume");
        let server = HttpServer::start();
        let mut config = config_for(&dir);
        config.cdns = vec![server.url("/cdn")];
        let content: Vec<u8> = (0..=255u8).collect();
        let md5 = crate::update::md5_bytes(&content);
        let path = res_path(&md5, content.len() as u64);
        server.route(&path, &content);

        let part = res_temp(&config, "data/a.bin", &md5, content.len() as u64, 0);
        fs::create_dir_all(part.parent().expect("parent")).expect("create staging");
        fs::write(&part, &content[..100]).expect("write partial");

        let mut plan = plan_of(vec![res_item("data/a.bin", &md5, content.len() as u64)]);
        plan.max_threads = 1;
        let report = apply_plan(&config, &plan, &ApplyOptions::default());
        assert!(report.results[0].ok, "{:?}", report.results[0].error);
        // 续传前缀与本次下载量一起构成完整文件的进度.
        assert_eq!(report.downloaded_bytes, content.len() as u64);
        assert_eq!(dir.read("install/data/a.bin"), content);
        assert!(!part.exists());
    }

    #[test]
    fn apply_plan_discards_unusable_partial_downloads() {
        let dir = TempDir::new("dl-discard");
        let server = HttpServer::start();
        let mut config = config_for(&dir);
        config.cdns = vec![server.url("/cdn")];
        let content = b"payload bytes";
        let md5 = crate::update::md5_bytes(content);
        let path = res_path(&md5, content.len() as u64);
        server.route(&path, content);

        let part = res_temp(&config, "data/a.bin", &md5, content.len() as u64, 0);
        fs::create_dir_all(part.parent().expect("parent")).expect("create staging");
        fs::write(&part, b"wrong content").expect("write broken part");
        let mut plan = plan_of(vec![res_item("data/a.bin", &md5, content.len() as u64)]);
        plan.max_threads = 1;
        let report = apply_plan(&config, &plan, &ApplyOptions::default());
        assert!(report.results[0].ok, "{:?}", report.results[0].error);
        assert_eq!(report.downloaded_bytes, content.len() as u64);
        assert_eq!(dir.read("install/data/a.bin"), content);

        let part = res_temp(&config, "data/b.bin", &md5, content.len() as u64, 0);
        fs::create_dir_all(part.parent().expect("parent")).expect("create staging");
        fs::write(&part, vec![0u8; content.len() + 5]).expect("write oversized part");
        let mut plan = plan_of(vec![res_item("data/b.bin", &md5, content.len() as u64)]);
        plan.max_threads = 1;
        let report = apply_plan(&config, &plan, &ApplyOptions::default());
        assert_eq!(report.downloaded_bytes, content.len() as u64);
        assert_eq!(dir.read("install/data/b.bin"), content);
    }

    #[test]
    fn apply_plan_reports_a_partial_download_that_cannot_be_resumed() {
        let dir = TempDir::new("dl-416");
        let server = HttpServer::start();
        let mut config = config_for(&dir);
        config.cdns = vec![server.url("/cdn")];
        let md5 = "ab".repeat(16);
        server.route(res_path(&md5, 100), b"short");

        let part = res_temp(&config, "data/a.bin", &md5, 100, 0);
        fs::create_dir_all(part.parent().expect("parent")).expect("create staging");
        fs::write(&part, vec![0u8; 50]).expect("write partial");

        let mut plan = plan_of(vec![res_item("data/a.bin", &md5, 100)]);
        plan.max_threads = 1;
        let report = apply_plan(&config, &plan, &ApplyOptions::default());
        assert_eq!(report.failed, 1);
        assert!(report.results[0]
            .error
            .as_deref()
            .expect("error text")
            .contains("download md5 mismatch"));
    }

    #[test]
    fn download_falls_back_to_the_next_cdn() {
        let dir = TempDir::new("dl-cdn");
        let server = HttpServer::start();
        let mut config = config_for(&dir);
        config.cdns = vec![server.url("/bad"), server.url("/good")];
        let content = b"payload bytes";
        let md5 = crate::update::md5_bytes(content);
        let suffix = format!(
            "{BRANCH}/Res/{}/{md5}.{}",
            md5.chars().next().unwrap_or('0'),
            content.len()
        );
        server.route(format!("/good/{suffix}"), content);

        let mut plan = plan_of(vec![res_item("data/a.bin", &md5, content.len() as u64)]);
        plan.max_threads = 1;
        let report = apply_plan(&config, &plan, &ApplyOptions::default());
        assert!(report.results[0].ok, "{:?}", report.results[0].error);
        assert_eq!(dir.read("install/data/a.bin"), content);
        assert!(server.hits(&format!("/bad/{suffix}")) >= 1);
        assert_eq!(server.hits(&format!("/good/{suffix}")), 1);
    }

    #[test]
    fn cdn_probe_needs_multiple_nodes_a_large_plan_and_no_limit() {
        assert!(cdn_probe_is_worthwhile(2, CDN_PROBE_MIN_BYTES, 0));
        assert!(!cdn_probe_is_worthwhile(1, CDN_PROBE_MIN_BYTES, 0));
        assert!(!cdn_probe_is_worthwhile(2, CDN_PROBE_MIN_BYTES - 1, 0));
        assert!(!cdn_probe_is_worthwhile(2, CDN_PROBE_MIN_BYTES, 4096));
    }

    #[test]
    fn cdn_probe_keeps_the_configured_order_when_nothing_responds() {
        let dir = TempDir::new("dl-probe-none");
        let server = HttpServer::start();
        let mut config = config_for(&dir);
        config.cdns = vec![server.url("/a"), server.url("/b")];
        let item = res_item("data/big.bin", &"ab".repeat(16), 128 * 1024);
        let http = HttpClient::new();

        assert!(rank_cdns(&config, &[&item], &http, &|| true).is_none());
        assert!(rank_cdns(&config, &[&item], &http, &|| false).is_none());
    }

    #[test]
    fn truncated_probe_samples_are_rejected() {
        let dir = TempDir::new("dl-probe-truncated");
        let server = HttpServer::start();
        let mut config = config_for(&dir);
        config.cdns = vec![server.url("/cdn")];
        let content = "w".repeat(256 * 1024);
        let md5 = crate::update::md5_bytes(content.as_bytes());
        let path = res_path(&md5, content.len() as u64);
        let url = res_url(&config, &md5, content.len() as u64).expect("probe url");
        let http = HttpClient::new();

        // 只返回一小段就正常结束: 样本不完整, 不能因为耗时短被当成快节点.
        server.route(&path, &content[..1024]);
        assert_eq!(probe_rate(&http, &url, CDN_PROBE_BYTES), 0);

        server.route(&path, &content);
        assert!(probe_rate(&http, &url, CDN_PROBE_BYTES) > 0);
    }

    #[test]
    fn cdn_probe_samples_the_pak_for_pak_entries() {
        let dir = TempDir::new("dl-probe-pak");
        let mut config = config_for(&dir);
        config.cdns = vec![
            "https://cdn.test/clientRes".to_string(),
            "https://backup.test/res".to_string(),
        ];
        let pak_size = 8 * 1024 * 1024;
        let item = pak_item(
            "Client/Content/Paks/pakchunk0.pak",
            &"cd".repeat(16),
            10,
            4,
            pak_size,
        );

        let (url, size) = cdn_probe_target(&config, &[&item]).expect("probe target");
        assert_eq!(size, pak_size);
        assert_eq!(
            url,
            format!(
                "https://cdn.test/clientRes/{BRANCH}/Res/a/{}.{pak_size}",
                "aa".repeat(16)
            )
        );
    }

    #[test]
    fn faster_cdn_is_picked_before_a_large_download() {
        let dir = TempDir::new("dl-probe-order");
        let slow = HttpServer::start();
        let fast = HttpServer::start();
        let mut config = config_for(&dir);
        config.cdns = vec![slow.url("/cdnA"), fast.url("/cdnB")];
        let content = "x".repeat(256 * 1024);
        let md5 = crate::update::md5_bytes(content.as_bytes());
        let suffix = format!(
            "{BRANCH}/Res/{}/{md5}.{}",
            md5.chars().next().unwrap_or('0'),
            content.len()
        );
        let slow_path = format!("/cdnA/{suffix}");
        let fast_path = format!("/cdnB/{suffix}");
        let (entered, release) = slow.route_blocked(&slow_path, &content);
        let gate = std::thread::spawn(move || {
            if entered.recv_timeout(Duration::from_secs(2)).is_ok() {
                // 拉开两边的采样耗时, 避免本机调度噪声决定排序.
                std::thread::sleep(Duration::from_millis(300));
                let _ = release.send(());
            }
        });
        fast.route(&fast_path, &content);

        let mut plan = plan_of(vec![res_item("data/big.bin", &md5, content.len() as u64)]);
        plan.max_threads = 1;
        let report = apply_plan(&config, &plan, &ApplyOptions::default());
        gate.join().expect("join probe gate");

        assert!(report.results[0].ok, "{:?}", report.results[0].error);
        assert_eq!(dir.read_text("install/data/big.bin"), content);
        // 慢节点只被采样一次, 真正的下载走了快节点.
        assert_eq!(slow.hits(&slow_path), 1);
        assert_eq!(fast.hits(&fast_path), 2);
    }

    #[test]
    fn disabling_the_probe_keeps_the_configured_order() {
        let dir = TempDir::new("dl-probe-off");
        let first = HttpServer::start();
        let second = HttpServer::start();
        let mut config = config_for(&dir);
        config.cdns = vec![first.url("/cdnA"), second.url("/cdnB")];
        let content = "y".repeat(256 * 1024);
        let md5 = crate::update::md5_bytes(content.as_bytes());
        let suffix = format!(
            "{BRANCH}/Res/{}/{md5}.{}",
            md5.chars().next().unwrap_or('0'),
            content.len()
        );
        let first_path = format!("/cdnA/{suffix}");
        let second_path = format!("/cdnB/{suffix}");
        first.route(&first_path, &content);
        second.route(&second_path, &content);

        let mut plan = plan_of(vec![res_item("data/big.bin", &md5, content.len() as u64)]);
        plan.max_threads = 1;
        let report = apply_plan(
            &config,
            &plan,
            &ApplyOptions {
                probe_cdns: false,
                ..Default::default()
            },
        );

        assert!(report.results[0].ok, "{:?}", report.results[0].error);
        assert_eq!(dir.read_text("install/data/big.bin"), content);
        // 关闭测速后不发采样请求, 直接走配置里的第一个节点.
        assert_eq!(first.hits(&first_path), 1);
        assert_eq!(second.hits(&second_path), 0);
    }

    #[test]
    fn download_verifies_the_content_md5() {
        let dir = TempDir::new("dl-md5");
        let server = HttpServer::start();
        let mut config = config_for(&dir);
        config.cdns = vec![server.url("/cdn")];
        let content = b"payload bytes";
        let md5 = "ff".repeat(16);
        let path = res_path(&md5, content.len() as u64);
        server.route(&path, content);

        let report = apply_plan(
            &config,
            &plan_of(vec![res_item("data/a.bin", &md5, content.len() as u64)]),
            &ApplyOptions::default(),
        );
        assert_eq!(report.failed, 1);
        assert!(report.results[0]
            .error
            .as_deref()
            .expect("error text")
            .contains("download md5 mismatch"));
        assert!(!dir.is_file("install/data/a.bin"));
        // 下载成功但校验失败: 下载字节有记录, 未通过的校验不计入耗时.
        assert!(report.timings.download_bytes > 0);
        assert_eq!(report.timings.verify_bytes, 0);
        assert_eq!(report.timings.install_bytes, 0);
    }

    #[test]
    fn pak_entries_are_downloaded_with_a_range_request() {
        let dir = TempDir::new("dl-pak");
        let server = HttpServer::start();
        let mut config = config_for(&dir);
        config.cdns = vec![server.url("/cdn")];
        let pak: Vec<u8> = (0..=255u8).collect();
        let entry_bytes = pak[10..40].to_vec();
        let entry_md5 = crate::update::md5_bytes(&entry_bytes);
        let pak_md5 = "aa".repeat(16);
        server.route(res_path(&pak_md5, pak.len() as u64), &pak);

        let report = apply_plan(
            &config,
            &plan_of(vec![pak_item(
                "Client/Content/Paks/pakchunk0.pak",
                &entry_md5,
                10,
                entry_bytes.len() as u64,
                pak.len() as u64,
            )]),
            &ApplyOptions::default(),
        );
        assert!(report.results[0].ok, "{:?}", report.results[0].error);
        assert_eq!(report.downloaded_bytes, entry_bytes.len() as u64);
        // 区间下载只统计卷内条目的字节.
        assert_eq!(report.timings.download_bytes, entry_bytes.len() as u64);
        assert_eq!(report.timings.verify_bytes, entry_bytes.len() as u64);
        assert_eq!(report.timings.install_bytes, entry_bytes.len() as u64);
        assert_eq!(
            dir.read("install/Client/Content/Paks/pakchunk0.pak"),
            entry_bytes
        );
        assert!(server.ranges().iter().any(|range| range == "bytes=10-39"));
    }

    #[test]
    fn pak_entries_with_bad_ids_are_rejected_before_downloading() {
        let dir = TempDir::new("dl-pak-bad");
        let server = HttpServer::start();
        let mut config = config_for(&dir);
        config.cdns = vec![server.url("/cdn")];

        let report = apply_plan(
            &config,
            &plan_of(vec![pak_item("data/a.pak", "short", 0, 4, 100)]),
            &ApplyOptions::default(),
        );
        assert!(report.results[0]
            .error
            .as_deref()
            .expect("error text")
            .contains("bad PatcherPak entry"));

        let report = apply_plan(
            &config,
            &plan_of(vec![pak_item(
                "data/b.pak",
                &"bb".repeat(16),
                u64::MAX,
                1,
                100,
            )]),
            &ApplyOptions::default(),
        );
        assert!(report.results[0]
            .error
            .as_deref()
            .expect("error text")
            .contains("bad PatcherPak entry"));
        assert!(server.targets().is_empty(), "{:?}", server.targets());
    }

    #[test]
    fn patches_are_applied_from_hdiff13() {
        let dir = TempDir::new("dl-patch");
        let server = HttpServer::start();
        let mut config = config_for(&dir);
        config.cdns = vec![server.url("/cdn")];
        let old_content = b"version one: reusable game bytes\n";
        let new_content = b"version two: reusable game bytes!\n";
        let old_md5 = crate::update::md5_bytes(old_content);
        let new_md5 = crate::update::md5_bytes(new_content);
        dir.write("install/data/a.bin", old_content);
        let patch_bytes = include_bytes!("../../tests/fixtures/reusable-game.hdiff");
        let patch_md5 = crate::update::md5_bytes(patch_bytes);
        server.route(res_path(&patch_md5, patch_bytes.len() as u64), patch_bytes);
        let item = patch_item(
            "data/a.bin",
            &old_md5,
            old_content.len() as u64,
            &new_md5,
            new_content.len() as u64,
            &[(&patch_md5, patch_bytes.len() as u64)],
        );

        let report = apply_plan(&config, &plan_of(vec![item]), &ApplyOptions::default());
        assert!(report.results[0].ok, "{:?}", report.results[0].error);
        assert_eq!(dir.read("install/data/a.bin"), new_content);
        assert_eq!(report.downloaded_bytes, patch_bytes.len() as u64);
        // 单跳补丁的应用字节就是最终产物大小, 下载耗时只覆盖补丁字节.
        assert_eq!(report.timings.apply_bytes, new_content.len() as u64);
        assert!(report.timings.apply_nanos > 0);
        assert!(report.timings.download_bytes >= patch_bytes.len() as u64);
        assert!(report.timings.verify_bytes > 0);
        assert!(report.timings.install_bytes > 0);
    }

    #[test]
    fn patch_apply_and_verify_run_outside_the_install_lock() {
        let dir = TempDir::new("dl-patch-lock-scope");
        let config = config_for(&dir);
        let old_content = b"version one: reusable game bytes\n";
        let new_content = b"version two: reusable game bytes!\n";
        let old_md5 = crate::update::md5_bytes(old_content);
        let new_md5 = crate::update::md5_bytes(new_content);
        dir.write("install/data/a.bin", old_content);
        let patch_bytes = include_bytes!("../../tests/fixtures/reusable-game.hdiff");
        let patch_md5 = crate::update::md5_bytes(patch_bytes);
        let item = patch_item(
            "data/a.bin",
            &old_md5,
            old_content.len() as u64,
            &new_md5,
            new_content.len() as u64,
            &[(&patch_md5, patch_bytes.len() as u64)],
        );

        // 补丁已下载完成, 让流程直接进入应用/校验/安装三个阶段.
        let staging = config.patcher_dir.join("tmp/nte-native");
        let patch_path = patch_temp_path(&staging, &item, &item.patches[0], 0);
        fs::create_dir_all(patch_path.parent().expect("patch parent")).expect("create staging");
        fs::write(&patch_path, patch_bytes).expect("write patch temp");

        // 取消回调随执行流同步调用, 用采样结果确认各阶段持有的锁与许可.
        let install_lock = std::sync::Arc::new(Mutex::new(()));
        let apply_gate = std::sync::Arc::new(ApplyGate::new(2, u64::MAX - PATCH_DISK_RESERVE));
        let samples = std::sync::Arc::new(Mutex::new(Vec::new()));
        let probe_lock = install_lock.clone();
        let probe_gate = apply_gate.clone();
        let probe_samples = samples.clone();
        let final_path = chain_final_temp(&staging, &item, 0).unwrap();
        let cancelled = move || {
            let locked = probe_lock.try_lock().is_err();
            let applying = probe_gate.active();
            probe_samples.lock().expect("record lock state").push((
                locked,
                applying,
                final_path.is_file(),
            ));
            false
        };
        let (tx, _rx) = mpsc::channel();
        let limiter = RateLimiter::new(0);
        let clock = DownloadClock::new();
        let progress = Progress::new(&tx, 0, 0, &cancelled, &limiter, &clock);
        let http = HttpClient::new();
        run_item(
            &config,
            &ApplyOptions::default(),
            &item,
            &config.install_root.join("data/a.bin"),
            &staging,
            &http,
            &install_lock,
            &apply_gate,
            &progress,
            false,
        )
        .expect("apply patch chain");
        assert_eq!(dir.read("install/data/a.bin"), new_content);

        let samples = samples.lock().expect("lock samples").clone();
        // 应用阶段持有 ApplyPermit, 但不再占用安装锁.
        assert!(
            samples
                .iter()
                .any(|(locked, applying, _)| !locked && *applying > 0),
            "应用补丁时不应持有安装锁: {samples:?}"
        );
        // 最终产物已生成后的校验仍在安装锁外, 且继续持有暂存许可.
        assert!(samples
            .iter()
            .any(|(locked, applying, created)| !locked && *applying > 0 && *created));
        assert!(samples
            .iter()
            .any(|(locked, applying, _)| *locked && *applying > 0));
        assert_eq!(apply_gate.active(), 0);
    }

    #[test]
    fn duplicate_items_use_separate_staging_paths() {
        let dir = TempDir::new("dl-duplicate-items");
        let server = HttpServer::start();
        let mut config = config_for(&dir);
        config.cdns = vec![server.url("/cdn")];
        config.max_threads = 4;
        let old_content = b"version one: reusable game bytes\n";
        let new_content = b"version two: reusable game bytes!\n";
        let old_md5 = crate::update::md5_bytes(old_content);
        let new_md5 = crate::update::md5_bytes(new_content);
        dir.write("install/data/a.bin", old_content);
        let patch_bytes = include_bytes!("../../tests/fixtures/reusable-game.hdiff");
        let patch_md5 = crate::update::md5_bytes(patch_bytes);
        server.route(res_path(&patch_md5, patch_bytes.len() as u64), patch_bytes);
        // 补丁链失败时会回退整包, 所以整包也要能下载.
        server.route(res_path(&new_md5, new_content.len() as u64), new_content);
        let item = patch_item(
            "data/a.bin",
            &old_md5,
            old_content.len() as u64,
            &new_md5,
            new_content.len() as u64,
            &[(&patch_md5, patch_bytes.len() as u64)],
        );
        let plan = plan_of(vec![item.clone(), item]);

        let report = apply_plan(&config, &plan, &ApplyOptions::default());
        assert_eq!(report.failed, 0, "{:?}", report.results);
        assert_eq!(report.results.len(), 2);
        assert!(
            report.results.iter().all(|result| result.ok),
            "{:?}",
            report.results
        );
        assert_eq!(dir.read("install/data/a.bin"), new_content);

        // 两个重复项各有独立暂存路径, 收尾后不应残留临时文件.
        let staging = config.patcher_dir.join("tmp/nte-native");
        let leftovers: Vec<String> = fs::read_dir(staging.join("Patch"))
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().to_string())
                    .collect()
            })
            .unwrap_or_default();
        assert!(leftovers.is_empty(), "暂存文件应清理干净: {leftovers:?}");
    }

    #[test]
    fn duplicate_items_get_distinct_staging_paths() {
        let dir = TempDir::new("dl-duplicate-tags");
        let config = config_for(&dir);
        let staging = config.patcher_dir.join("tmp/nte-native");
        let patch_bytes = include_bytes!("../../tests/fixtures/reusable-game.hdiff");
        let patch_md5 = crate::update::md5_bytes(patch_bytes);
        let item = patch_item(
            "data/a.bin",
            &crate::update::md5_bytes(b"old"),
            3,
            &crate::update::md5_bytes(b"new"),
            3,
            &[(&patch_md5, patch_bytes.len() as u64)],
        );
        // 同一路径的不同条目必须落到不同暂存路径.
        assert_ne!(item_tag(&item, 0), item_tag(&item, 1));
        assert_ne!(
            patch_temp_path(&staging, &item, &item.patches[0], 0),
            patch_temp_path(&staging, &item, &item.patches[0], 1)
        );
        assert_ne!(
            chain_temp(&staging, &item, 0, 0),
            chain_temp(&staging, &item, 0, 1)
        );
        assert_ne!(
            chain_final_temp(&staging, &item, 0),
            chain_final_temp(&staging, &item, 1)
        );

        // 两个重复项都在计划里时, 清理不能把任何一方的暂存当成过期删除.
        fs::create_dir_all(staging.join("Patch")).expect("create patch staging");
        let first = patch_temp_path(&staging, &item, &item.patches[0], 0);
        let second = patch_temp_path(&staging, &item, &item.patches[0], 1);
        fs::write(&first, b"x").expect("write first part");
        fs::write(&second, b"x").expect("write second part");
        let final_temp = chain_final_temp(&staging, &item, 0).unwrap();
        let obsolete_temp = chain_temp(&staging, &item, 0, 10);
        fs::write(&final_temp, b"output").expect("keep reusable output");
        fs::write(&obsolete_temp, b"obsolete output").expect("stale chain output");
        let plan = plan_of(vec![item.clone(), item]);
        cleanup_stale_parts(&staging, &plan);
        assert!(first.exists());
        assert!(second.exists());
        assert!(final_temp.exists(), "当前计划的链产物应保留供续传");
        assert!(!obsolete_temp.exists(), "旧条目的链产物应回收");
    }

    #[test]
    fn tag_indices_do_not_depend_on_only_selection() {
        let _dir = TempDir::new("dl-tag-stability");
        let old_md5 = crate::update::md5_bytes(b"old");
        let new_md5 = crate::update::md5_bytes(b"new");
        let patch_md5 = "cc".repeat(16);
        let item = patch_item("data/a.bin", &old_md5, 3, &new_md5, 3, &[(&patch_md5, 10)]);
        let other = res_item("data/b.bin", &"dd".repeat(16), 5);
        let plan = plan_of(vec![item.clone(), other, item]);
        let all: Vec<&UpdateItem> = plan.items.iter().collect();
        let all_tags = selected_tag_indices(&plan, &all);
        assert_eq!(all_tags, vec![0, 0, 1]);

        // --only data/a.bin 选中两个同路径项, 序号仍应保持 0/1.
        let only: Vec<&UpdateItem> = plan
            .items
            .iter()
            .filter(|item| item.path.contains("data/a.bin"))
            .collect();
        let only_tags = selected_tag_indices(&plan, &only);
        assert_eq!(only_tags, vec![0, 1]);
        assert_eq!(
            item_tag(all[0], all_tags[0]),
            item_tag(only[0], only_tags[0])
        );
        assert_eq!(
            item_tag(all[2], all_tags[2]),
            item_tag(only[1], only_tags[1])
        );
        assert!(std::ptr::eq(all[0], only[0]));
    }

    #[test]
    fn apply_gate_blocks_above_its_limit_and_wakes_on_drop() {
        let gate = ApplyGate::new(2, u64::MAX - PATCH_DISK_RESERVE);
        let first = gate.acquire();
        let second = gate.acquire();
        assert_eq!(gate.active(), 2);

        let (tx, rx) = mpsc::channel();
        std::thread::scope(|scope| {
            let gate = &gate;
            let tx = tx.clone();
            scope.spawn(move || {
                let _permit = gate.acquire();
                tx.send(()).expect("signal permit");
            });
            // 许可已满时, 第三路必须阻塞.
            assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
            drop(second);
            rx.recv_timeout(Duration::from_secs(2))
                .expect("third acquire after release");
        });
        drop(first);
        assert_eq!(gate.active(), 0);

        // 释放后可以继续获取.
        let again = gate.acquire();
        assert_eq!(gate.active(), 1);
        drop(again);
        assert_eq!(gate.active(), 0);
    }

    #[test]
    fn apply_gate_wait_observes_cancellation() {
        let gate = ApplyGate::new(4, 500);
        let permit = gate
            .acquire_cancellable(500, &|| Ok(u64::MAX), &|| false)
            .unwrap();
        let cancelled = std::sync::Arc::new(AtomicBool::new(false));
        let started = Instant::now();
        let (tx, rx) = mpsc::channel();
        let stopped_while_waiting = std::thread::scope(|scope| {
            let gate = &gate;
            let flag = cancelled.clone();
            let handle = scope.spawn(move || {
                let result = gate.acquire_cancellable(
                    500,
                    &|| {
                        tx.send(()).unwrap();
                        Ok(u64::MAX)
                    },
                    &|| flag.load(Ordering::Relaxed),
                );
                matches!(result, Err(Error::Cancelled))
            });
            rx.recv_timeout(Duration::from_secs(2))
                .expect("workspace admission attempted");
            cancelled.store(true, Ordering::Relaxed);
            handle.join().expect("join gate waiter")
        });
        assert!(stopped_while_waiting, "等待许可时应响应取消");
        assert!(started.elapsed() < Duration::from_secs(2));

        // 取消不影响许可的释放与后续获取.
        drop(permit);
        assert_eq!(gate.active(), 0);
        let again = gate.acquire();
        assert_eq!(gate.active(), 1);
        drop(again);
        // 持续取消时不会再等到许可.
        assert!(matches!(
            gate.acquire_cancellable(0, &|| Ok(u64::MAX), &|| true),
            Err(Error::Cancelled)
        ));
    }

    #[test]
    fn apply_limit_uses_half_the_cores_up_to_four() {
        assert_eq!(apply_limit_for(1), 1);
        assert_eq!(apply_limit_for(3), 1);
        assert_eq!(apply_limit_for(4), 2);
        assert_eq!(apply_limit_for(8), 4);
        assert_eq!(apply_limit_for(64), 4);
    }

    #[test]
    fn apply_gate_reserves_workspace_until_the_permit_is_dropped() {
        // 剩余空间只容得下一条链, 即使第一条尚未写盘也不能放行第二条.
        let gate = ApplyGate::new(4, 500);
        let available = || Ok(PATCH_DISK_RESERVE + 500);
        let first = gate
            .acquire_cancellable(500, &available, &|| false)
            .unwrap();
        let (tx, rx) = mpsc::channel();
        let (attempt_tx, attempt_rx) = mpsc::channel();
        std::thread::scope(|scope| {
            let gate = &gate;
            let available = &available;
            scope.spawn(move || {
                let _permit = gate
                    .acquire_cancellable(
                        500,
                        &|| {
                            attempt_tx.send(()).unwrap();
                            available()
                        },
                        &|| false,
                    )
                    .unwrap();
                tx.send(()).unwrap();
            });
            attempt_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("second admission attempted");
            assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
            assert_eq!(gate.lock().reserved, 500);
            drop(first);
            rx.recv_timeout(Duration::from_secs(2))
                .expect("workspace released");
        });
        assert_eq!(gate.lock().reserved, 0);

        // 无其它链可等待时直接返回错误, 不会永久阻塞.
        assert!(gate
            .acquire_cancellable(501, &available, &|| false)
            .is_err());
        assert!(gate
            .acquire_cancellable(500, &|| Ok(PATCH_DISK_RESERVE + 499), &|| false)
            .is_err());
        assert!(gate
            .acquire_cancellable(u64::MAX, &|| Ok(u64::MAX), &|| false)
            .is_err());
    }

    #[test]
    fn apply_gate_admits_multiple_chains_within_the_shared_budget() {
        let gate = ApplyGate::new(4, 1000);
        let first = gate
            .acquire_cancellable(400, &|| Ok(u64::MAX), &|| false)
            .unwrap();
        let second = gate
            .acquire_cancellable(600, &|| Ok(u64::MAX), &|| false)
            .unwrap();
        assert_eq!(gate.active(), 2);
        assert_eq!(gate.lock().reserved, 1000);
        assert!(matches!(
            gate.acquire_cancellable(1, &|| Ok(u64::MAX), &|| true),
            Err(Error::Cancelled)
        ));
        drop(first);
        drop(second);
        assert_eq!(gate.lock().reserved, 0);
    }

    #[test]
    fn chain_workspace_uses_the_adjacent_output_peak() {
        let mut item = patch_item(
            "data/a.bin",
            &"aa".repeat(16),
            10,
            &"bb".repeat(16),
            30,
            &[(&"cc".repeat(16), 4), (&"dd".repeat(16), 5)],
        );
        assert_eq!(item.patch_workspace(), 30 + 30);
        // 多跳时按相邻两跳产物之和取峰值.
        item.patches[0].new_size = 100;
        item.patches[1].new_size = 300;
        assert_eq!(item.patch_workspace(), 400);

        let single = patch_item(
            "data/b.bin",
            &"ee".repeat(16),
            10,
            &"ff".repeat(16),
            42,
            &[(&"11".repeat(16), 3)],
        );
        assert_eq!(single.patch_workspace(), 42);
        assert_eq!(
            res_item("data/c.bin", &"22".repeat(16), 7).patch_workspace(),
            0
        );
    }

    #[test]
    fn patch_fallback_does_not_download_past_the_workspace_budget() {
        let dir = TempDir::new("dl-patch-space");
        let server = HttpServer::start();
        let mut config = config_for(&dir);
        config.cdns = vec![server.url("/cdn")];
        let old = b"old game bytes";
        let new = b"new game bytes";
        let patch = b"invalid patch bytes";
        let patch_md5 = crate::update::md5_bytes(patch);
        let item = patch_item(
            "data/a.bin",
            &crate::update::md5_bytes(old),
            old.len() as u64,
            &crate::update::md5_bytes(new),
            new.len() as u64,
            &[(&patch_md5, patch.len() as u64)],
        );
        dir.write("install/data/a.bin", old);
        let staging = config.patcher_dir.join("tmp/nte-native");
        let cached = patch_temp_path(&staging, &item, &item.patches[0], 0);
        fs::create_dir_all(cached.parent().unwrap()).unwrap();
        fs::write(cached, patch).unwrap();
        server.route(res_path(&item.md5, item.size), new);
        let gate = ApplyGate::new(4, 0);
        let (tx, _rx) = mpsc::channel();
        let clock = DownloadClock::new();
        let limiter = RateLimiter::new(0);
        let progress = Progress::new(&tx, 0, 0, &|| false, &limiter, &clock);
        let result = run_item(
            &config,
            &ApplyOptions::default(),
            &item,
            &config.install_root.join("data/a.bin"),
            &staging,
            &HttpClient::new(),
            &Mutex::new(()),
            &gate,
            &progress,
            false,
        );
        assert!(matches!(result, Err(Error::Integrity(_))));
        assert!(server.targets().is_empty(), "空间不足时不应继续下载整包");
        assert_eq!(dir.read("install/data/a.bin"), old);
        assert_eq!(gate.active(), 0);
    }

    #[test]
    fn patch_failures_fall_back_to_the_full_file() {
        let dir = TempDir::new("dl-patch-fallback");
        let server = HttpServer::start();
        let mut config = config_for(&dir);
        config.cdns = vec![server.url("/cdn")];
        let new_content = b"full content";
        let new_md5 = crate::update::md5_bytes(new_content);
        dir.write("install/data/a.bin", b"old");
        server.route(res_path(&new_md5, new_content.len() as u64), new_content);
        let item = patch_item(
            "data/a.bin",
            &crate::update::md5_bytes(b"old"),
            3,
            &new_md5,
            new_content.len() as u64,
            &[(&"ee".repeat(16), 4)],
        );

        let report = apply_plan(&config, &plan_of(vec![item]), &ApplyOptions::default());
        assert!(report.results[0].ok, "{:?}", report.results[0].error);
        assert_eq!(dir.read("install/data/a.bin"), new_content);
    }

    #[test]
    fn an_invalid_patch_is_rejected() {
        let dir = TempDir::new("dl-patch-bad");
        let server = HttpServer::start();
        let mut config = config_for(&dir);
        config.cdns = vec![server.url("/cdn")];
        let new_content = b"full content";
        let new_md5 = crate::update::md5_bytes(new_content);
        dir.write("install/data/a.bin", b"old");
        let patch_bytes = b"diff";
        let patch_md5 = crate::update::md5_bytes(patch_bytes);
        server.route(res_path(&patch_md5, patch_bytes.len() as u64), patch_bytes);
        server.route(res_path(&new_md5, new_content.len() as u64), new_content);
        let item = patch_item(
            "data/a.bin",
            &crate::update::md5_bytes(b"old"),
            3,
            &new_md5,
            new_content.len() as u64,
            &[(&patch_md5, patch_bytes.len() as u64)],
        );

        let report = apply_plan(&config, &plan_of(vec![item]), &ApplyOptions::default());
        assert!(report.results[0].ok, "{:?}", report.results[0].error);
        assert_eq!(dir.read("install/data/a.bin"), new_content);
    }

    #[test]
    fn applies_a_two_step_patch_chain() {
        let dir = TempDir::new("dl-chain");
        let server = HttpServer::start();
        let mut config = config_for(&dir);
        config.cdns = vec![server.url("/cdn")];
        let old_content = b"alpha beta gamma delta\n".repeat(200);
        let mut mid_content = String::from_utf8(old_content.clone())
            .expect("utf8")
            .replace("gamma", "GAMMA")
            .into_bytes();
        for _ in 0..10 {
            mid_content.extend_from_slice(b"middle addition\n");
        }
        let mut new_content = String::from_utf8(mid_content.clone())
            .expect("utf8")
            .replace("delta", "DELTA")
            .into_bytes();
        for _ in 0..5 {
            new_content.extend_from_slice(b"final addition\n");
        }

        let old_md5 = crate::update::md5_bytes(&old_content);
        let mid_md5 = crate::update::md5_bytes(&mid_content);
        let new_md5 = crate::update::md5_bytes(&new_content);
        dir.write("install/data/a.bin", &old_content);
        let mid_patch = include_bytes!("../../tests/fixtures/chain-mid.hdiff");
        let final_patch = include_bytes!("../../tests/fixtures/chain-final.hdiff");
        let mid_patch_md5 = crate::update::md5_bytes(mid_patch);
        let final_patch_md5 = crate::update::md5_bytes(final_patch);
        server.route(res_path(&mid_patch_md5, mid_patch.len() as u64), mid_patch);
        server.route(
            res_path(&final_patch_md5, final_patch.len() as u64),
            final_patch,
        );
        let item = patch_item(
            "data/a.bin",
            &old_md5,
            old_content.len() as u64,
            &new_md5,
            new_content.len() as u64,
            &[
                (&mid_patch_md5, mid_patch.len() as u64),
                (&final_patch_md5, final_patch.len() as u64),
            ],
        );
        // 第二个补丁的输入是中间内容，不是本地旧文件。
        let mut item = item;
        item.patches[0].new_md5 = mid_md5.clone();
        item.patches[0].new_size = mid_content.len() as u64;
        item.patches[1].old_md5 = mid_md5;
        item.patches[1].old_size = mid_content.len() as u64;

        let report = apply_plan(&config, &plan_of(vec![item]), &ApplyOptions::default());
        assert!(report.results[0].ok, "{:?}", report.results[0].error);
        assert_eq!(dir.read("install/data/a.bin"), new_content);
        assert_eq!(
            report.downloaded_bytes,
            (mid_patch.len() + final_patch.len()) as u64
        );
        // 多跳时应用字节按各跳产物累计, 不是最终文件大小.
        assert_eq!(
            report.timings.apply_bytes,
            (mid_content.len() + new_content.len()) as u64
        );
        assert!(report.timings.download_bytes >= (mid_patch.len() + final_patch.len()) as u64);
    }

    #[test]
    fn range_download_resumes_from_the_block_index() {
        let dir = TempDir::new("dl-block-index");
        let server = HttpServer::start();
        let mut config = config_for(&dir);
        config.cdns = vec![server.url("/cdn")];
        let content: Vec<u8> = (0..200_000u32).map(|value| value as u8).collect();
        let md5 = crate::update::md5_bytes(&content);
        server.route(res_path(&md5, content.len() as u64), &content);

        let part = res_temp(&config, "data/a.bin", &md5, content.len() as u64, 0);
        fs::create_dir_all(part.parent().expect("parent")).expect("create staging");
        let file = File::create(&part).expect("create sparse part");
        file.set_len(content.len() as u64).expect("set length");
        use std::os::unix::fs::FileExt;
        file.write_all_at(&content[..RANGE_CHUNK as usize], 0)
            .expect("write first chunk");
        let index = range_index_path(&part);
        write_range_index_header(&index, content.len() as u64, 4).expect("header");
        append_range_index(&index, 0).expect("done chunk");

        let report = apply_plan(
            &config,
            &plan_of(vec![res_item("data/a.bin", &md5, content.len() as u64)]),
            &ApplyOptions::default(),
        );
        assert!(report.results[0].ok, "{:?}", report.results[0].error);
        assert_eq!(dir.read("install/data/a.bin"), content);
        // 已完成的块与本次下载的块一起计入进度.
        assert_eq!(report.downloaded_bytes, content.len() as u64);
        // 已完成的块来自上一次任务, 不计入本次下载耗时; 校验仍覆盖整个文件.
        assert!(report.timings.download_bytes > 0);
        assert!(report.timings.download_bytes < content.len() as u64);
        assert_eq!(report.timings.verify_bytes, content.len() as u64);
        let ranges = server.ranges();
        assert_eq!(ranges.len(), 3, "{ranges:?}");
        assert!(!ranges.iter().any(|range| range.starts_with("bytes=0-")));
        assert!(!index.exists());
    }

    #[test]
    fn range_download_uses_manifest_blocks() {
        let dir = TempDir::new("dl-blocks");
        let server = HttpServer::start();
        let mut config = config_for(&dir);
        config.cdns = vec![server.url("/cdn")];
        let content: Vec<u8> = (0..200_000u32).map(|value| value as u8).collect();
        let md5 = crate::update::md5_bytes(&content);
        server.route(res_path(&md5, content.len() as u64), &content);
        let mut item = res_item("data/a.bin", &md5, content.len() as u64);
        let UpdateKind::Res { res } = &mut item.kind else {
            unreachable!()
        };
        for index in 0..4u64 {
            let start = index * 50_000;
            res.blocks.push(Block {
                index: index as u32,
                start,
                size: 50_000,
                md5: crate::update::md5_bytes(&content[start as usize..(start + 50_000) as usize]),
            });
        }

        let task_started = Instant::now();
        let report = apply_plan(&config, &plan_of(vec![item]), &ApplyOptions::default());
        let task_elapsed = task_started.elapsed();
        assert!(report.results[0].ok, "{:?}", report.results[0].error);
        assert_eq!(dir.read("install/data/a.bin"), content);
        assert_eq!(report.downloaded_bytes, content.len() as u64);
        // 所有块都是本次下载, 校验覆盖整个文件.
        assert_eq!(report.timings.download_bytes, content.len() as u64);
        assert_eq!(report.timings.verify_bytes, content.len() as u64);
        // 下载耗时是任务级活跃墙钟, 不会超过整个任务的实际墙钟.
        assert!(report.timings.download_nanos > 0);
        assert!(
            report.timings.download_nanos
                <= u64::try_from(task_elapsed.as_nanos()).unwrap_or(u64::MAX)
        );
        assert_eq!(server.ranges().len(), 4, "{:?}", server.ranges());
    }

    #[test]
    fn cleanup_stale_parts_removes_unreferenced_downloads() {
        let dir = TempDir::new("dl-cleanup");
        let config = config_for(&dir);
        let staging = config.patcher_dir.join("tmp/nte-native");
        fs::create_dir_all(staging.join("Res")).expect("create res staging");
        let md5 = "aa".repeat(16);
        let plan = plan_of(vec![
            res_item("data/a.bin", &md5, 1),
            res_item("data/other.bin", &"bb".repeat(16), 2),
        ]);
        // 计划内的暂存与续传索引都要保留.
        let keep = res_temp(&config, "data/a.bin", &md5, 1, 0);
        fs::write(&keep, b"x").expect("keep part");
        let keep_index = range_index_path(&keep);
        fs::write(&keep_index, b"x").expect("keep idx");
        // 过期条目的路径应被清理.
        let stale_index = res_temp(&config, "data/a.bin", &md5, 1, 1);
        fs::write(&stale_index, b"x").expect("stale index");
        let stale_path = staging.join(format!("Res/{md5}.1.ffff.part"));
        fs::write(&stale_path, b"x").expect("stale path tag");
        let stale_md5 = staging.join("Res/bb.2.part");
        fs::write(&stale_md5, b"x").expect("stale md5");
        // 未选中的计划项不参与本轮, 保留其暂存以便之后续传.
        let untouched = staging.join(format!(
            "Res/{}.2.{}.part",
            "bb".repeat(16),
            item_tag_for("data/other.bin", 0)
        ));
        fs::write(&untouched, b"x").expect("keep unselected item part");

        cleanup_stale_parts(&staging, &plan);
        assert!(keep.exists());
        assert!(keep_index.exists());
        assert!(untouched.exists(), "未选中计划项的暂存应保留");
        assert!(!stale_index.exists());
        assert!(!stale_path.exists());
        assert!(!stale_md5.exists());
    }

    #[test]
    fn reports_download_verify_and_install_stages() {
        let dir = TempDir::new("dl-stages");
        let server = HttpServer::start();
        let mut config = config_for(&dir);
        config.cdns = vec![server.url("/cdn")];
        let content = b"payload bytes";
        let md5 = crate::update::md5_bytes(content);
        server.route(res_path(&md5, content.len() as u64), content);
        let mut stages = Vec::new();
        let report = apply_plan_observed_with_stage(
            &config,
            &plan_of(vec![res_item("data/a.bin", &md5, content.len() as u64)]),
            &ApplyOptions::default(),
            &mut |_| {},
            &mut |_, _| {},
            &mut |_, stage| stages.push(stage),
            &|| false,
        );
        assert!(report.results[0].ok);
        assert!(stages.contains(&ApplyStage::Download));
        assert!(stages.contains(&ApplyStage::Verify));
        assert!(stages.contains(&ApplyStage::Install));
    }

    #[test]
    fn download_clock_counts_overlapping_reads_once() {
        let clock = DownloadClock::new();
        let barrier = std::sync::Barrier::new(3);
        let held: Vec<Duration> = std::thread::scope(|scope| {
            let mut workers = Vec::new();
            for _ in 0..2 {
                let clock = &clock;
                let barrier = &barrier;
                workers.push(scope.spawn(move || {
                    barrier.wait();
                    let started = Instant::now();
                    let _guard = clock.begin();
                    std::thread::sleep(Duration::from_millis(100));
                    started.elapsed()
                }));
            }
            barrier.wait();
            workers
                .into_iter()
                .map(|worker| worker.join().expect("join download clock worker"))
                .collect()
        });
        let wall = clock.wall_nanos();
        let sum: u64 = held
            .iter()
            .map(|duration| u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX))
            .sum();
        // 两段读取几乎完全重叠: 活跃墙钟接近单段, 明显小于两段之和.
        assert!(wall >= Duration::from_millis(100).as_nanos() as u64);
        assert!(wall * 2 < sum * 3 / 2, "wall={wall} sum={sum}");
        // 所有守卫结束后墙钟不再增长.
        let settled = clock.wall_nanos();
        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(clock.wall_nanos(), settled);
    }

    #[test]
    fn progress_accumulates_stage_timings_for_the_worker_report() {
        let (tx, _rx) = mpsc::channel();
        let cancelled = || false;
        let limiter = RateLimiter::new(0);
        let clock = DownloadClock::new();
        let progress = Progress::new(&tx, 0, 0, &cancelled, &limiter, &clock);

        progress.add_download(100);
        progress.add_apply(200, Duration::from_millis(3));
        progress.add_verify(300, Duration::from_millis(4));
        progress.add_install(400, Duration::from_millis(5));

        let timings = progress.timings();
        assert_eq!(timings.download_bytes, 100);
        // 下载耗时由任务级时钟在汇总时写入, 单项汇总不重复累计.
        assert_eq!(timings.download_nanos, 0);
        assert_eq!(timings.apply_bytes, 200);
        assert_eq!(timings.apply_nanos, 3_000_000);
        assert_eq!(timings.verify_bytes, 300);
        assert_eq!(timings.verify_nanos, 4_000_000);
        assert_eq!(timings.install_bytes, 400);
        assert_eq!(timings.install_nanos, 5_000_000);
    }

    #[test]
    fn multiple_items_are_downloaded_and_installed() {
        let dir = TempDir::new("dl-parallel");
        let server = HttpServer::start();
        let mut config = config_for(&dir);
        config.max_threads = 4;
        config.cdns = vec![server.url("/cdn")];
        let mut items = Vec::new();
        let mut expected = Vec::new();
        for index in 0..4u8 {
            let content = format!("parallel payload {index}").into_bytes();
            let md5 = crate::update::md5_bytes(&content);
            let path = format!("data/{index}.bin");
            server.route(res_path(&md5, content.len() as u64), &content);
            expected.push((path.clone(), content));
            items.push(res_item(&path, &md5, 18));
        }
        let report = apply_plan(&config, &plan_of(items), &ApplyOptions::default());
        assert_eq!(report.failed, 0);
        assert_eq!(report.results.len(), 4);
        for (path, content) in expected {
            assert_eq!(dir.read(format!("install/{path}")), content);
        }
        // 多文件并行时各阶段字节汇总到同一份报告; 每个 payload 18 字节.
        assert_eq!(report.timings.download_bytes, 4 * 18);
        assert_eq!(report.timings.install_bytes, 4 * 18);
    }

    #[test]
    fn cancellation_after_patch_creation_keeps_the_installed_file() {
        let dir = TempDir::new("patch-final-cancel");
        let server = HttpServer::start();
        let mut config = config_for(&dir);
        config.cdns = vec![server.url("/cdn")];
        let old = b"version one: reusable game bytes\n";
        let new = b"version two: reusable game bytes!\n";
        dir.write("install/data/a.bin", old);
        let bytes = include_bytes!("../../tests/fixtures/reusable-game.hdiff");
        let hash = crate::update::md5_bytes(bytes);
        server.route(res_path(&hash, bytes.len() as u64), bytes);
        let item = patch_item(
            "data/a.bin",
            &crate::update::md5_bytes(old),
            old.len() as u64,
            &crate::update::md5_bytes(new),
            new.len() as u64,
            &[(&hash, bytes.len() as u64)],
        );
        let final_path =
            chain_final_temp(&config.patcher_dir.join("tmp/nte-native"), &item, 0).unwrap();
        let cancelled = || final_path.is_file();
        let plan = plan_of(vec![item]);
        let report = apply_plan_observed_full(
            &config,
            &plan,
            &ApplyOptions::default(),
            0,
            &mut |_| {},
            &mut |_, _| {},
            &mut |_, _| {},
            &mut |_| {},
            &cancelled,
        );
        assert!(report.cancelled);
        assert_eq!(dir.read("install/data/a.bin"), old);
        assert!(!config.patcher_dir.join("ResList.xml").exists());
        assert_eq!(server.targets().len(), 1);
        // 取消发生在校验阶段, 已完成的下载和应用耗时仍如实记录.
        assert!(report.timings.download_bytes > 0);
        assert!(report.timings.apply_bytes > 0);
        assert_eq!(report.timings.install_bytes, 0);

        // 再次执行直接复用完整补丁产物, 不计入新的网络请求.
        let report = apply_plan(&config, &plan, &ApplyOptions::default());
        assert_eq!(report.failed, 0);
        assert_eq!(report.downloaded_bytes, bytes.len() as u64);
        assert_eq!(dir.read("install/data/a.bin"), new);
        assert_eq!(server.targets().len(), 1);
    }
}
