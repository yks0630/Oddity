//! 按预计完成时间选择补丁链或整包下载。
//!
//! 规划阶段已经按补丁字节选出最省下载量的链；这里再用实测或默认吞吐
//! 估算两条路径的完成时间，避免在快速网络或慢速存储上为了省字节反而更慢。
//! 校验和安装两条路径都要做一次，估算时同时计入并相互抵消。

use crate::distribution::Distribution;
use crate::game::check::UpdateItem;

/// 默认下载吞吐（字节/秒）。实测 2026-11-02 国服单连接 5.9–6.7 MB/s、
/// 4–8 连接聚合 10–12 MB/s；这里取偏保守的聚合值。
const DEFAULT_DOWNLOAD_CN: f64 = 8_000_000.0;
const DEFAULT_DOWNLOAD_GLOBAL: f64 = 6_000_000.0;
/// 补丁应用吞吐。真实小差分样本实测 1.6–1.9 GB/s，合成大差分样本 1.1 GB/s，
/// 首次更新取保守值，之后由实际应用样本校准。
const DEFAULT_APPLY: f64 = 1_000_000_000.0;
/// 整文件 MD5 校验吞吐；实测 365 MB/s。
const DEFAULT_VERIFY: f64 = 360_000_000.0;

/// 每跳补丁的固定开销（创建/删除临时文件等），实测在 0.1 s 以内。
const PER_HOP_SECONDS: f64 = 0.05;
/// 只有整包预计明显更快时才放弃补丁，避免在网络抖动时来回切换。
const SWITCH_MARGIN: f64 = 0.10;
const SWITCH_MIN_SECONDS: f64 = 1.0;

const EWMA_ALPHA: f64 = 0.3;
const DOWNLOAD_LIMITS: (f64, f64) = (250_000.0, 200_000_000.0);
const APPLY_LIMITS: (f64, f64) = (50_000_000.0, 5_000_000_000.0);
const VERIFY_LIMITS: (f64, f64) = (50_000_000.0, 2_000_000_000.0);

/// 一次应用任务各阶段的累计耗时，用于校准吞吐估计。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StageTimings {
    pub download_bytes: u64,
    pub download_nanos: u64,
    pub apply_bytes: u64,
    pub apply_nanos: u64,
    pub verify_bytes: u64,
    pub verify_nanos: u64,
    pub install_bytes: u64,
    pub install_nanos: u64,
}

impl StageTimings {
    pub fn add(&mut self, other: &StageTimings) {
        self.download_bytes = self.download_bytes.saturating_add(other.download_bytes);
        self.download_nanos = self.download_nanos.saturating_add(other.download_nanos);
        self.apply_bytes = self.apply_bytes.saturating_add(other.apply_bytes);
        self.apply_nanos = self.apply_nanos.saturating_add(other.apply_nanos);
        self.verify_bytes = self.verify_bytes.saturating_add(other.verify_bytes);
        self.verify_nanos = self.verify_nanos.saturating_add(other.verify_nanos);
        self.install_bytes = self.install_bytes.saturating_add(other.install_bytes);
        self.install_nanos = self.install_nanos.saturating_add(other.install_nanos);
    }

    pub fn is_empty(&self) -> bool {
        self.download_bytes == 0
            && self.download_nanos == 0
            && self.apply_bytes == 0
            && self.apply_nanos == 0
            && self.verify_bytes == 0
            && self.verify_nanos == 0
            && self.install_bytes == 0
            && self.install_nanos == 0
    }
}

/// 更新各阶段的吞吐估计（字节/秒）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Throughput {
    pub download_bytes_per_second: f64,
    pub apply_bytes_per_second: f64,
    pub verify_bytes_per_second: f64,
}

impl Default for Throughput {
    fn default() -> Self {
        Self::for_app_id("1289")
    }
}

impl Throughput {
    pub fn for_app_id(app_id: &str) -> Self {
        let distribution = app_id
            .parse::<i64>()
            .ok()
            .and_then(Distribution::from_game_id);
        let download = match distribution {
            Some(Distribution::Global) => DEFAULT_DOWNLOAD_GLOBAL,
            _ => DEFAULT_DOWNLOAD_CN,
        };
        Self {
            download_bytes_per_second: download,
            apply_bytes_per_second: DEFAULT_APPLY,
            verify_bytes_per_second: DEFAULT_VERIFY,
        }
    }

    /// 用一次任务的实际耗时更新估计；样本为空或异常时保持原值。
    pub fn observe(&mut self, timings: &StageTimings) {
        observe_rate(
            &mut self.download_bytes_per_second,
            timings.download_bytes,
            timings.download_nanos,
            DOWNLOAD_LIMITS,
        );
        observe_rate(
            &mut self.apply_bytes_per_second,
            timings.apply_bytes,
            timings.apply_nanos,
            APPLY_LIMITS,
        );
        observe_rate(
            &mut self.verify_bytes_per_second,
            timings.verify_bytes,
            timings.verify_nanos,
            VERIFY_LIMITS,
        );
    }
}

fn observe_rate(current: &mut f64, bytes: u64, nanos: u64, limits: (f64, f64)) {
    if bytes == 0 || nanos == 0 {
        return;
    }
    let rate = (bytes as f64 * 1_000_000_000.0 / nanos as f64).clamp(limits.0, limits.1);
    *current = EWMA_ALPHA * rate + (1.0 - EWMA_ALPHA) * *current;
}

/// 单个文件的补丁链与整包时间估算。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TimeEstimate {
    pub patch_seconds: f64,
    pub full_seconds: f64,
    pub patch_download_bytes: u64,
    pub patch_apply_bytes: u64,
    pub hops: usize,
}

pub fn estimate(item: &UpdateItem, throughput: &Throughput) -> TimeEstimate {
    let download = throughput.download_bytes_per_second.max(1.0);
    let apply = throughput.apply_bytes_per_second.max(1.0);
    let verify = throughput.verify_bytes_per_second.max(1.0);
    let full_seconds = item.size as f64 / download + item.size as f64 / verify;
    if item.patches.is_empty() {
        return TimeEstimate {
            patch_seconds: full_seconds,
            full_seconds,
            patch_download_bytes: item.size,
            patch_apply_bytes: 0,
            hops: 0,
        };
    }
    let apply_bytes: u64 = item.patches.iter().map(|patch| patch.new_size).sum();
    let hops = item.patches.len();
    let patch_seconds = item.download_size as f64 / download
        + apply_bytes as f64 / apply
        + hops as f64 * PER_HOP_SECONDS
        + item.size as f64 / verify;
    TimeEstimate {
        patch_seconds,
        full_seconds,
        patch_download_bytes: item.download_size,
        patch_apply_bytes: apply_bytes,
        hops,
    }
}

/// 预计整包明显更快时返回 false，规划阶段应放弃补丁链。
pub fn should_use_patch(item: &UpdateItem, throughput: &Throughput) -> bool {
    if item.patches.is_empty() {
        return false;
    }
    let estimate = estimate(item, throughput);
    estimate.patch_seconds <= estimate.full_seconds * (1.0 + SWITCH_MARGIN) + SWITCH_MIN_SECONDS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::check::{PatchRef, UpdateKind, UpdateReason};
    use crate::game::manifest::ResEntry;

    fn res_item(size: u64, patches: Vec<PatchRef>) -> UpdateItem {
        let mut item = UpdateItem {
            path: "data/a.bin".into(),
            size,
            md5: "aa".repeat(16),
            reason: UpdateReason::Size,
            kind: UpdateKind::Res {
                res: ResEntry {
                    filename: "data/a.bin".into(),
                    filesize: size,
                    md5: "aa".repeat(16),
                    ..Default::default()
                },
            },
            patches: Vec::new(),
            download_size: size,
        };
        if !patches.is_empty() {
            item.download_size = patches.iter().map(|patch| patch.size).sum();
            item.patches = patches;
        }
        item
    }

    fn patch(size: u64, new_size: u64) -> PatchRef {
        PatchRef {
            md5: "bb".repeat(16),
            size,
            old_md5: "cc".repeat(16),
            old_size: new_size,
            new_md5: "dd".repeat(16),
            new_size,
        }
    }

    #[test]
    fn defaults_follow_the_distribution() {
        let cn = Throughput::for_app_id("1289");
        let global = Throughput::for_app_id("3000001");
        let unknown = Throughput::for_app_id("nope");
        assert_eq!(cn.download_bytes_per_second, DEFAULT_DOWNLOAD_CN);
        assert_eq!(global.download_bytes_per_second, DEFAULT_DOWNLOAD_GLOBAL);
        assert_eq!(unknown.download_bytes_per_second, DEFAULT_DOWNLOAD_CN);
        assert_eq!(Throughput::default(), cn);
    }

    #[test]
    fn estimates_chain_and_full_file_time() {
        let throughput = Throughput {
            download_bytes_per_second: 10_000_000.0,
            apply_bytes_per_second: 1_000_000_000.0,
            verify_bytes_per_second: 360_000_000.0,
        };
        let item = res_item(1_000_000_000, vec![patch(20_000_000, 1_000_000_000)]);
        let estimate = estimate(&item, &throughput);
        assert_eq!(estimate.hops, 1);
        assert_eq!(estimate.patch_download_bytes, 20_000_000);
        assert_eq!(estimate.patch_apply_bytes, 1_000_000_000);
        assert!((estimate.patch_seconds - (2.0 + 1.0 + 0.05 + 2.777_777)).abs() < 1e-3);
        assert!((estimate.full_seconds - (100.0 + 2.777_777)).abs() < 1e-3);
        assert!(should_use_patch(&item, &throughput));
    }

    #[test]
    fn fast_download_with_slow_apply_uses_the_full_file() {
        let throughput = Throughput {
            download_bytes_per_second: 1_000_000_000.0,
            apply_bytes_per_second: 50_000_000.0,
            verify_bytes_per_second: 1_000_000_000.0,
        };
        let item = res_item(
            1_000_000_000,
            vec![
                patch(100_000_000, 1_000_000_000),
                patch(10_000_000, 1_000_000_000),
            ],
        );
        let estimate = estimate(&item, &throughput);
        assert!(estimate.patch_seconds > estimate.full_seconds * 2.0);
        assert!(!should_use_patch(&item, &throughput));
    }

    #[test]
    fn slow_download_keeps_the_patch_chain() {
        let throughput = Throughput {
            download_bytes_per_second: 1_000_000.0,
            apply_bytes_per_second: 1_000_000_000.0,
            verify_bytes_per_second: 1_000_000_000.0,
        };
        let item = res_item(1_000_000_000, vec![patch(100_000_000, 1_000_000_000)]);
        assert!(should_use_patch(&item, &throughput));
    }

    #[test]
    fn tiny_files_keep_the_patch_despite_the_margin() {
        let throughput = Throughput {
            download_bytes_per_second: 1_000_000_000.0,
            apply_bytes_per_second: 1_000_000.0,
            verify_bytes_per_second: 1_000_000_000.0,
        };
        let item = res_item(1024, vec![patch(900, 1024)]);
        // 整包确实更快，但差距不足 1 秒，避免为微小收益增加风险。
        assert!(should_use_patch(&item, &throughput));
    }

    #[test]
    fn estimates_the_full_file_when_there_is_no_chain() {
        let throughput = Throughput {
            download_bytes_per_second: 10_000_000.0,
            apply_bytes_per_second: 1_000_000_000.0,
            verify_bytes_per_second: 360_000_000.0,
        };
        let item = res_item(1_000_000_000, Vec::new());
        let estimate = estimate(&item, &throughput);
        assert_eq!(estimate.patch_download_bytes, item.size);
        assert_eq!(estimate.patch_apply_bytes, 0);
        assert_eq!(estimate.hops, 0);
        assert_eq!(estimate.patch_seconds, estimate.full_seconds);
        assert!((estimate.full_seconds - (100.0 + 2.777_777)).abs() < 1e-3);
    }
    #[test]
    fn items_without_a_chain_never_use_patch() {
        let item = res_item(1_000_000, Vec::new());
        assert!(!should_use_patch(&item, &Throughput::default()));
    }

    #[test]
    fn observations_update_rates_with_ewma() {
        let mut throughput = Throughput {
            download_bytes_per_second: 10_000_000.0,
            apply_bytes_per_second: 1_000_000_000.0,
            verify_bytes_per_second: 360_000_000.0,
        };
        let timings = StageTimings {
            download_bytes: 20_000_000,
            download_nanos: 1_000_000_000,
            apply_bytes: 2_000_000_000,
            apply_nanos: 1_000_000_000,
            verify_bytes: 1_000_000_000,
            verify_nanos: 1_000_000_000,
            ..Default::default()
        };
        throughput.observe(&timings);
        assert!((throughput.download_bytes_per_second - 13_000_000.0).abs() < 1.0);
        assert!((throughput.apply_bytes_per_second - 1_300_000_000.0).abs() < 1.0);
        assert!((throughput.verify_bytes_per_second - 552_000_000.0).abs() < 1.0);
    }

    #[test]
    fn observations_ignore_empty_samples_and_clamp_outliers() {
        let mut throughput = Throughput::default();
        let before = throughput;
        throughput.observe(&StageTimings::default());
        assert_eq!(throughput, before);

        let timings = StageTimings {
            download_bytes: 1,
            download_nanos: 1,
            apply_bytes: u64::MAX,
            apply_nanos: 1,
            verify_bytes: 0,
            verify_nanos: 5,
            ..Default::default()
        };
        throughput.observe(&timings);
        assert_eq!(
            throughput.download_bytes_per_second,
            EWMA_ALPHA * DOWNLOAD_LIMITS.1 + (1.0 - EWMA_ALPHA) * before.download_bytes_per_second
        );
        assert_eq!(
            throughput.apply_bytes_per_second,
            EWMA_ALPHA * APPLY_LIMITS.1 + (1.0 - EWMA_ALPHA) * before.apply_bytes_per_second
        );
        assert_eq!(
            throughput.verify_bytes_per_second,
            before.verify_bytes_per_second
        );
    }

    #[test]
    fn stage_timings_accumulate_and_detect_empty() {
        let mut total = StageTimings::default();
        assert!(total.is_empty());
        total.add(&StageTimings {
            download_bytes: 10,
            download_nanos: 20,
            apply_bytes: 30,
            apply_nanos: 40,
            verify_bytes: 50,
            verify_nanos: 60,
            install_bytes: 70,
            install_nanos: 80,
        });
        let sample = total;
        total.add(&sample);
        assert!(!total.is_empty());
        assert_eq!(total.download_bytes, 20);
        assert_eq!(total.apply_nanos, 80);
        assert_eq!(total.install_bytes, 140);
    }
}
