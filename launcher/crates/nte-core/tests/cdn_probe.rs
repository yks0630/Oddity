use nte_core::game::check::{UpdateItem, UpdateKind, UpdatePlan, UpdateReason};
use nte_core::game::config::GameUpdateConfig;
use nte_core::game::download::{
    apply_plan, apply_plan_observed, apply_plan_observed_full, ApplyOptions,
};
use nte_core::game::manifest::ResEntry;
use nte_core::update::md5_bytes;

#[allow(dead_code)]
#[path = "../src/testing.rs"]
mod testing;
use testing::{HttpServer, TempDir};

const BRANCH: &str = "publish_ob";
const PROBE_MIN: usize = 32 * 1024 * 1024;
const PROBE_RANGE: &str = "bytes=0-2097151";

fn config(dir: &TempDir, server: &HttpServer) -> GameUpdateConfig {
    GameUpdateConfig {
        launcher_root: dir.join("install/NTELauncher"),
        install_root: dir.join("install"),
        config_dir: dir.join("install/NTELauncher/ResFilesM/1289/PatcherConfig"),
        patcher_dir: dir.join("install/NTELauncher/UserData/Patcher/PatcherSDK"),
        branch: BRANCH.into(),
        update_branch: "publish_Updata".into(),
        app_id: "1289".into(),
        cdns: vec![server.url("/first"), server.url("/second")],
        rules: Vec::new(),
        check_dirs: Vec::new(),
        max_threads: 1,
    }
}

fn resource(path: &str, md5: &str, size: u64) -> UpdateItem {
    UpdateItem {
        path: path.into(),
        size,
        md5: md5.into(),
        reason: UpdateReason::Missing,
        kind: UpdateKind::Res {
            res: ResEntry {
                filename: path.into(),
                filesize: size,
                md5: md5.into(),
                ..Default::default()
            },
        },
        patches: Vec::new(),
        download_size: size,
    }
}

fn plan(items: Vec<UpdateItem>) -> UpdatePlan {
    UpdatePlan {
        items,
        max_threads: 1,
        ..Default::default()
    }
}

fn route(cdn: &str, md5: &str, size: usize) -> String {
    format!("/{cdn}/{BRANCH}/Res/{}/{md5}.{size}", &md5[..1])
}

fn options() -> ApplyOptions {
    ApplyOptions {
        update_ledger: false,
        ..Default::default()
    }
}

#[test]
fn probes_largest_resource_with_bounded_ranges_and_honors_cancellation() {
    let dir = TempDir::new("probe-target");
    let server = HttpServer::start();
    let config = config(&dir, &server);
    let small_md5 = "11".repeat(16);
    let large_md5 = "22".repeat(16);
    let sample = vec![1; 4096];
    for cdn in ["first", "second"] {
        server.route(route(cdn, &large_md5, PROBE_MIN), &sample);
    }
    let plan = plan(vec![
        resource("data/small.bin", &small_md5, 1024),
        resource("data/large.bin", &large_md5, PROBE_MIN as u64),
    ]);
    let mut progress = 0;
    let report = apply_plan_observed(
        &config,
        &plan,
        &options(),
        &mut |_| {},
        &mut |_, bytes| progress += bytes,
        &|| server.targets().len() >= 2,
    );

    assert!(report.cancelled);
    assert_eq!(progress, 0);
    assert_eq!(
        server.targets(),
        vec![
            route("first", &large_md5, PROBE_MIN),
            route("second", &large_md5, PROBE_MIN)
        ]
    );
    assert_eq!(server.ranges(), vec![PROBE_RANGE, PROBE_RANGE]);
}

#[test]
fn failed_probe_is_deprioritized_and_samples_do_not_count_as_download_progress() {
    let dir = TempDir::new("probe-failure");
    let server = HttpServer::start();
    let config = config(&dir, &server);
    let content = vec![b'x'; PROBE_MIN];
    let md5 = md5_bytes(&content);
    let first = route("first", &md5, content.len());
    let second = route("second", &md5, content.len());
    server.route(&second, &content);
    let plan = plan(vec![resource("data/file.bin", &md5, content.len() as u64)]);
    let mut progress = 0;
    let report = apply_plan_observed(
        &config,
        &plan,
        &options(),
        &mut |_| {},
        &mut |_, bytes| progress += bytes,
        &|| false,
    );

    assert_eq!(report.failed, 0, "{:?}", report.results);
    assert_eq!(dir.read("install/data/file.bin"), content);
    assert_eq!(server.targets(), vec![first, second.clone(), second]);
    assert_eq!(server.ranges(), vec![PROBE_RANGE, PROBE_RANGE, ""]);
    assert_eq!(report.downloaded_bytes, PROBE_MIN as u64);
    assert_eq!(progress, PROBE_MIN as u64);
}

#[test]
fn only_filter_excludes_unselected_bytes_from_probe_threshold() {
    let dir = TempDir::new("probe-selected");
    let server = HttpServer::start();
    let config = config(&dir, &server);
    let content = b"selected";
    let md5 = md5_bytes(content);
    let first = route("first", &md5, content.len());
    server.route(&first, content);
    let plan = plan(vec![
        resource("data/selected.bin", &md5, content.len() as u64),
        resource("data/other.bin", &"ab".repeat(16), PROBE_MIN as u64),
    ]);
    let opts = ApplyOptions {
        only: Some("selected".into()),
        ..options()
    };
    let report = apply_plan(&config, &plan, &opts);

    assert_eq!(report.failed, 0, "{:?}", report.results);
    assert_eq!(dir.read("install/data/selected.bin"), content);
    assert_eq!(server.targets(), vec![first]);
    assert_eq!(server.ranges(), vec![""]);
}

#[test]
fn bandwidth_limit_skips_probes_for_a_large_download() {
    let dir = TempDir::new("probe-limit");
    let server = HttpServer::start();
    let config = config(&dir, &server);
    let content = vec![b'y'; PROBE_MIN];
    let md5 = md5_bytes(&content);
    let first = route("first", &md5, content.len());
    server.route(&first, &content);
    let plan = plan(vec![resource("data/file.bin", &md5, content.len() as u64)]);
    let report = apply_plan_observed_full(
        &config,
        &plan,
        &options(),
        64 * 1024 * 1024,
        &mut |_| {},
        &mut |_, _| {},
        &mut |_, _| {},
        &mut |_| {},
        &|| false,
    );

    assert_eq!(report.failed, 0, "{:?}", report.results);
    assert_eq!(dir.read("install/data/file.bin"), content);
    assert_eq!(server.targets(), vec![first]);
    assert_eq!(server.ranges(), vec![""]);
}

#[test]
fn samples_are_limited_to_the_resource_size() {
    let dir = TempDir::new("probe-short-target");
    let server = HttpServer::start();
    let config = config(&dir, &server);
    let size = 1024 * 1024;
    let md5 = "ab".repeat(16);
    for cdn in ["first", "second"] {
        server.route(route(cdn, &md5, size), vec![1; 4096]);
    }
    let items = (0..32)
        .map(|index| resource(&format!("data/{index}.bin"), &md5, size as u64))
        .collect();
    let report = apply_plan_observed(
        &config,
        &plan(items),
        &options(),
        &mut |_| {},
        &mut |_, _| {},
        &|| server.targets().len() >= 2,
    );

    assert!(report.cancelled);
    assert_eq!(
        server.targets(),
        vec![route("first", &md5, size), route("second", &md5, size)]
    );
    assert_eq!(server.ranges(), vec!["bytes=0-1048575", "bytes=0-1048575"]);
}

#[test]
fn download_just_below_threshold_does_not_probe() {
    let dir = TempDir::new("probe-below-threshold");
    let server = HttpServer::start();
    let config = config(&dir, &server);
    let content = vec![b'z'; PROBE_MIN - 1];
    let md5 = md5_bytes(&content);
    let first = route("first", &md5, content.len());
    server.route(&first, &content);
    let report = apply_plan(
        &config,
        &plan(vec![resource("data/file.bin", &md5, content.len() as u64)]),
        &options(),
    );

    assert_eq!(report.failed, 0, "{:?}", report.results);
    assert_eq!(dir.read("install/data/file.bin"), content);
    assert_eq!(server.targets(), vec![first]);
    assert_eq!(server.ranges(), vec![""]);
}
