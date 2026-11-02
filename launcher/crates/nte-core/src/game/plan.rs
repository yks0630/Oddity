//! 补丁链与整包下载的成本模型。
//!
//! check.rs 用这里的吞吐估计给 lastdiff 图的边定价：补丁链按「下载时间 +
//! 每跳应用时间 + 每跳固定开销」选链，避免在快速网络或慢速存储上为了省
//! 字节反而更慢。校验两条路径都要做一次，估算时同时计入并相互抵消。

use std::collections::VecDeque;

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

/// 规划级处理通道数默认值。
///
/// 补丁应用与最终校验目前都在全局安装锁内串行执行，整份计划只能按 1 条
/// 处理通道估计；执行层把补丁处理并行化后可以按实际通道数调整。
pub const PLAN_PROCESSING_LANES_DEFAULT: usize = 1;

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
    let apply_bytes: u64 = item
        .patches
        .iter()
        .map(|patch| patch.new_size)
        .fold(0, u64::saturating_add);
    let hops = item.patches.len();
    let patch_seconds =
        item.download_size as f64 / download + patch_processing_seconds(item, apply, verify);
    TimeEstimate {
        patch_seconds,
        full_seconds,
        patch_download_bytes: item.download_size,
        patch_apply_bytes: apply_bytes,
        hops,
    }
}

/// 整份更新计划的预计完成时间（秒）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlanEstimate {
    /// 纯下载资源耗时, 不含 worker 内校验.
    pub download_seconds: f64,
    /// 补丁应用与校验的累计通道耗时.
    pub processing_seconds: f64,
    pub makespan_seconds: f64,
}

/// 按执行层的 worker 占用顺序估计完工时间.
///
/// 下载共享聚合吞吐, 整包下载后在原 worker 校验; 补丁下载后等待处理通道,
/// 应用和校验结束才能领取下一项. 逐个推进阶段完成事件, 保留单文件收尾时间.
/// 不包含缓存命中、失败重试、安装耗时和并发磁盘争用, 不是实际运行 ETA.
pub fn estimate_plan(
    items: &[UpdateItem],
    throughput: &Throughput,
    processing_lanes: usize,
    download_workers: usize,
) -> PlanEstimate {
    enum Stage {
        Download(f64),
        Waiting,
        Processing(f64),
        Verify(f64),
    }
    let download = throughput.download_bytes_per_second.max(1.0);
    let apply = throughput.apply_bytes_per_second.max(1.0);
    let verify = throughput.verify_bytes_per_second.max(1.0);
    let worker_count = download_workers.clamp(1, 8).min(items.len());
    let lanes = processing_lanes.max(1).min(worker_count.max(1));
    let mut workers: Vec<Option<(usize, Stage)>> = (0..worker_count).map(|_| None).collect();
    let mut waiting: VecDeque<usize> = VecDeque::new();
    let mut next_item = 0;
    let mut elapsed = 0.0;
    loop {
        for worker in &mut workers {
            if worker.is_none() && next_item < items.len() {
                *worker = Some((
                    next_item,
                    Stage::Download(items[next_item].download_size as f64),
                ));
                next_item += 1;
            }
        }
        let processing_count = workers
            .iter()
            .filter(|worker| matches!(worker, Some((_, Stage::Processing(_)))))
            .count();
        for _ in processing_count..lanes {
            let Some(worker_index) = waiting.pop_front() else {
                break;
            };
            let (item_index, stage) = workers[worker_index].as_mut().expect("waiting worker");
            *stage =
                Stage::Processing(patch_processing_seconds(&items[*item_index], apply, verify));
        }
        let downloading = workers
            .iter()
            .filter(|worker| matches!(worker, Some((_, Stage::Download(_)))))
            .count();
        let rate = download / downloading.max(1) as f64;
        let event = workers
            .iter()
            .enumerate()
            .filter_map(|(index, worker)| {
                let (_, stage) = worker.as_ref()?;
                let seconds = match stage {
                    Stage::Download(bytes) => *bytes / rate,
                    Stage::Processing(seconds) | Stage::Verify(seconds) => *seconds,
                    Stage::Waiting => return None,
                };
                Some((index, seconds))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
        let Some((finished, seconds)) = event else {
            break;
        };
        elapsed += seconds;
        for (_, stage) in workers.iter_mut().flatten() {
            match stage {
                Stage::Download(bytes) => *bytes = (*bytes - seconds * rate).max(0.0),
                Stage::Processing(remaining) | Stage::Verify(remaining) => {
                    *remaining = (*remaining - seconds).max(0.0)
                }
                Stage::Waiting => (),
            }
        }
        let (item_index, stage) = workers[finished].take().expect("finished worker");
        if matches!(stage, Stage::Download(_)) {
            let item = &items[item_index];
            let stage = if item.has_patch() {
                waiting.push_back(finished);
                Stage::Waiting
            } else {
                Stage::Verify(item.size as f64 / verify)
            };
            workers[finished] = Some((item_index, stage));
        }
    }
    PlanEstimate {
        download_seconds: items
            .iter()
            .map(|item| item.download_size as f64 / download)
            .sum(),
        processing_seconds: items
            .iter()
            .filter(|item| item.has_patch())
            .map(|item| patch_processing_seconds(item, apply, verify))
            .sum::<f64>()
            / lanes as f64,
        makespan_seconds: elapsed,
    }
}

/// 复用同一份计划估计, 逐轮选择使整体完工时间明显缩短的整包替换.
/// 与单文件决策使用相同的切换余量, 避免为微小的估计差异增加下载量.
pub fn balance_plan_choices(
    items: &mut [UpdateItem],
    throughput: &Throughput,
    processing_lanes: usize,
    download_workers: usize,
) -> usize {
    let mut switched = 0;
    loop {
        let current =
            estimate_plan(items, throughput, processing_lanes, download_workers).makespan_seconds;
        let mut best = None;
        for index in 0..items.len() {
            if !items[index].has_patch() {
                continue;
            }
            let patches = std::mem::take(&mut items[index].patches);
            let patch_bytes = items[index].download_size;
            items[index].download_size = items[index].size;
            let next = estimate_plan(items, throughput, processing_lanes, download_workers)
                .makespan_seconds;
            items[index].patches = patches;
            items[index].download_size = patch_bytes;
            if current > next * (1.0 + SWITCH_MARGIN) + SWITCH_MIN_SECONDS
                && best.is_none_or(|(_, known)| next < known)
            {
                best = Some((index, next));
            }
        }
        let Some((index, _)) = best else { break };
        items[index].patches.clear();
        items[index].download_size = items[index].size;
        switched += 1;
    }
    switched
}

fn patch_processing_seconds(item: &UpdateItem, apply: f64, verify: f64) -> f64 {
    item.patches
        .iter()
        .map(|patch| patch.new_size as f64 / apply + PER_HOP_SECONDS)
        .sum::<f64>()
        + item.size as f64 / verify
}

/// 单跳补丁的预计耗时（纳秒，饱和到 u64::MAX）。
///
/// 包含补丁下载、该跳输出内容的应用时间与每跳固定开销。下载/应用吞吐为
/// 0、负数或 NaN 时按 1 B/s 处理，避免除零与 NaN 传播。
pub fn patch_edge_nanos(patch_bytes: u64, output_bytes: u64, throughput: &Throughput) -> u64 {
    let download = throughput.download_bytes_per_second.max(1.0);
    let apply = throughput.apply_bytes_per_second.max(1.0);
    nanos_for(patch_bytes, download)
        .saturating_add(nanos_for(output_bytes, apply))
        .saturating_add(seconds_to_nanos(PER_HOP_SECONDS))
}

/// 链时间的剪枝上界：整包时间加切换余量，超过它的链不会被保留。
pub fn chain_upper_bound_nanos(item_size: u64, throughput: &Throughput) -> u64 {
    let download = item_size as f64 / throughput.download_bytes_per_second.max(1.0);
    let verify = item_size as f64 / throughput.verify_bytes_per_second.max(1.0);
    // 边权未含最终校验, 但切换余量按包含校验的整包耗时计算.
    seconds_to_nanos(download * (1.0 + SWITCH_MARGIN) + verify * SWITCH_MARGIN + SWITCH_MIN_SECONDS)
}

/// 字节数按吞吐折算为纳秒，饱和处理。
fn nanos_for(bytes: u64, bytes_per_second: f64) -> u64 {
    let nanos = bytes as f64 * 1_000_000_000.0 / bytes_per_second;
    if nanos >= u64::MAX as f64 {
        u64::MAX
    } else {
        nanos.round() as u64
    }
}

fn seconds_to_nanos(seconds: f64) -> u64 {
    let nanos = seconds * 1_000_000_000.0;
    if nanos >= u64::MAX as f64 {
        u64::MAX
    } else {
        nanos.round() as u64
    }
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

    #[test]
    fn costs_convert_to_nanoseconds() {
        let throughput = Throughput {
            download_bytes_per_second: 10_000_000.0,
            apply_bytes_per_second: 1_000_000_000.0,
            verify_bytes_per_second: 365_000_000.0,
        };
        // 20 MB 下载 2s + 1 GB 应用 1s + 每跳 0.05s。
        assert_eq!(
            patch_edge_nanos(20_000_000, 1_000_000_000, &throughput),
            3_050_000_000
        );
        assert_eq!(
            chain_upper_bound_nanos(1_000_000_000, &throughput),
            111_273_972_603
        );
    }

    #[test]
    fn costs_handle_degenerate_rates_and_saturate() {
        let degenerate = Throughput {
            download_bytes_per_second: 0.0,
            apply_bytes_per_second: f64::NAN,
            verify_bytes_per_second: f64::NAN,
        };
        let one_byte_per_second = Throughput {
            download_bytes_per_second: 1.0,
            apply_bytes_per_second: 1.0,
            verify_bytes_per_second: 1.0,
        };
        // 0 与 NaN 都按 1 B/s 处理。
        assert_eq!(
            patch_edge_nanos(1, 1, &degenerate),
            patch_edge_nanos(1, 1, &one_byte_per_second)
        );
        // 饱和而不是回绕。
        assert_eq!(
            patch_edge_nanos(u64::MAX, u64::MAX, &one_byte_per_second),
            u64::MAX
        );
        assert_eq!(
            chain_upper_bound_nanos(u64::MAX, &one_byte_per_second),
            u64::MAX
        );
    }

    #[test]
    fn estimates_plan_makespan_and_scales_processing_lanes() {
        let throughput = Throughput {
            download_bytes_per_second: 10_000_000.0,
            apply_bytes_per_second: 1_000_000_000.0,
            verify_bytes_per_second: 365_000_000.0,
        };
        let items = vec![
            res_item(1_000_000_000, vec![patch(20_000_000, 1_000_000_000)]),
            res_item(500_000_000, Vec::new()),
        ];
        let one_lane = estimate_plan(&items, &throughput, 1, 4);
        // 下载共 52s; 整包随后校验 1.3699s. 补丁在 4s 下载完成,
        // 应用 1s + 每跳开销 0.05s + 校验 2.7397s, 早于整包结束.
        assert!((one_lane.download_seconds - 52.0).abs() < 1e-3);
        assert!((one_lane.processing_seconds - 3.789_726).abs() < 1e-3);
        assert!((one_lane.makespan_seconds - 53.369_863).abs() < 1e-3);

        let two_lanes = estimate_plan(&items, &throughput, 2, 4);
        assert!((two_lanes.processing_seconds - 1.894_863).abs() < 1e-3);
        // 通道数至少为 1，0 与 1 等价。
        assert_eq!(
            estimate_plan(&items, &throughput, 0, 4).processing_seconds,
            one_lane.processing_seconds
        );
        let patches = vec![
            res_item(1_000_000_000, vec![patch(20_000_000, 1_000_000_000)]),
            res_item(1_000_000_000, vec![patch(20_000_000, 1_000_000_000)]),
        ];
        let serial = estimate_plan(&patches, &throughput, 1, 4);
        let parallel = estimate_plan(&patches, &throughput, 2, 4);
        assert!((serial.makespan_seconds - 11.579_452).abs() < 1e-3);
        assert!((parallel.makespan_seconds - 7.789_726).abs() < 1e-3);
    }

    #[test]
    fn realistic_rates_keep_every_patch_choice() {
        let throughput = Throughput {
            download_bytes_per_second: 8_000_000.0,
            apply_bytes_per_second: 1_000_000_000.0,
            verify_bytes_per_second: 365_000_000.0,
        };
        let mut items = vec![
            res_item(6_000_000_000, vec![patch(20_000_000, 6_000_000_000)]),
            res_item(6_000_000_000, vec![patch(20_000_000, 6_000_000_000)]),
        ];
        // 真实 8 MB/s 下载下整包要多下 12 GB，换掉的串行处理只有约 45s。
        assert_eq!(balance_plan_choices(&mut items, &throughput, 1, 4), 0);
        assert!(items.iter().all(|item| item.patches.len() == 1));
        assert_eq!(items[0].download_size, 20_000_000);
    }

    #[test]
    fn balances_plan_under_fast_download_and_slow_apply() {
        let throughput = Throughput {
            download_bytes_per_second: 200_000_000.0,
            apply_bytes_per_second: 100_000_000.0,
            verify_bytes_per_second: 1_000_000_000.0,
        };
        let mut items = vec![
            res_item(4_000_000_000, vec![patch(100_000_000, 4_000_000_000)]),
            res_item(500_000_000, vec![patch(10_000_000, 500_000_000)]),
        ];
        let before = estimate_plan(&items, &throughput, 1, 4).makespan_seconds;
        let switched = balance_plan_choices(&mut items, &throughput, 1, 4);
        let after = estimate_plan(&items, &throughput, 1, 4).makespan_seconds;
        // 4 GB 项处理约 44s、0.5 GB 项约 5.5s；换整包分别多下 19.5s/2.45s，
        // 先换大项; 再换小项只会增加下载与收尾耗时.
        assert_eq!(switched, 1);
        assert!(items[0].patches.is_empty());
        assert_eq!(items[0].download_size, 4_000_000_000);
        assert_eq!(items[1].patches.len(), 1);
        assert!(after < before);
    }

    #[test]
    fn plan_balance_ignores_items_without_chains() {
        let throughput = Throughput::default();
        let mut items = vec![
            res_item(1_000_000_000, Vec::new()),
            res_item(2_000_000_000, Vec::new()),
        ];
        assert_eq!(
            estimate_plan(&items, &throughput, 1, 4).processing_seconds,
            0.0
        );
        assert_eq!(balance_plan_choices(&mut items, &throughput, 1, 4), 0);
        assert!(items.iter().all(|item| item.patches.is_empty()));
    }

    #[test]
    fn fast_download_keeps_patch_when_full_verify_is_not_free() {
        // 补丁 0.1s 下载 + 0.5s 应用 + 0.05s 固定开销 + 1s 校验.
        // 整包 1s 下载 + 1s 校验, 应保留补丁.
        let throughput = Throughput {
            download_bytes_per_second: 1_000_000_000.0,
            apply_bytes_per_second: 2_000_000_000.0,
            verify_bytes_per_second: 1_000_000_000.0,
        };
        let mut items = vec![res_item(
            1_000_000_000,
            vec![patch(100_000_000, 1_000_000_000)],
        )];
        let before = estimate_plan(&items, &throughput, 1, 1);
        assert!((before.processing_seconds - 1.55).abs() < 1e-6);
        assert!((before.makespan_seconds - 1.65).abs() < 1e-6);
        assert_eq!(balance_plan_choices(&mut items, &throughput, 1, 1), 0);
        assert_eq!(items[0].patches.len(), 1);
        let after = estimate_plan(&items, &throughput, 1, 1);
        assert!(after.makespan_seconds >= before.makespan_seconds);
    }

    #[test]
    fn single_item_plan_includes_download_processing_and_verification() {
        let throughput = Throughput::default();
        for item in [
            res_item(1_000_000_000, vec![patch(100_000_000, 1_000_000_000)]),
            res_item(1_000_000_000, vec![]),
        ] {
            let expected = estimate(&item, &throughput);
            for workers in [1, 8] {
                let plan = estimate_plan(std::slice::from_ref(&item), &throughput, 1, workers);
                assert!((plan.makespan_seconds - expected.patch_seconds).abs() < 1e-6);
            }
        }
    }

    #[test]
    fn adding_a_tiny_full_file_does_not_dilute_large_file_verification() {
        let throughput = Throughput {
            download_bytes_per_second: 1_000_000_000.0,
            apply_bytes_per_second: 1_000_000_000.0,
            verify_bytes_per_second: 100_000_000.0,
        };
        let mut items = vec![res_item(1_000_000_000, vec![])];
        let before = estimate_plan(&items, &throughput, 1, 8).makespan_seconds;
        items.push(res_item(1, vec![]));
        let after = estimate_plan(&items, &throughput, 1, 8).makespan_seconds;
        assert!(after >= before);
        assert!((before - 11.0).abs() < 1e-6);
    }

    #[test]
    fn workers_wait_for_patch_processing_before_downloading_the_next_item() {
        let throughput = Throughput {
            download_bytes_per_second: 100.0,
            apply_bytes_per_second: 100.0,
            verify_bytes_per_second: 100.0,
        };
        let items = vec![
            res_item(1000, vec![patch(100, 1000)]),
            res_item(1000, vec![]),
        ];
        // 单 worker: 补丁 1 + 10 + 0.05 + 10, 再整包 10 + 10.
        assert!((estimate_plan(&items, &throughput, 1, 1).makespan_seconds - 41.05).abs() < 1e-6);
        // 两 worker: 补丁 2s 下完, 处理到 22.05s; 整包 11s 下完, 21s 校验完.
        assert!((estimate_plan(&items, &throughput, 1, 2).makespan_seconds - 22.05).abs() < 1e-6);
    }

    #[test]
    fn tiny_model_improvements_keep_the_patch() {
        let throughput = Throughput {
            download_bytes_per_second: 1000.0,
            apply_bytes_per_second: 950.0,
            verify_bytes_per_second: 1000.0,
        };
        let mut items = vec![res_item(1000, vec![patch(900, 1000)])];
        assert_eq!(balance_plan_choices(&mut items, &throughput, 1, 1), 0);
        assert!(items[0].has_patch());
    }
}
