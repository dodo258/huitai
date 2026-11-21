//! 本机实跑：读三家真实额度，打印每个渠道的结果和耗时（会联网请求一次 Claude 用量接口）。
//! 默认跳过；手动跑：`cargo test --test quota_live -- --ignored --nocapture`。

use std::time::Instant;

use huitai::core::Vendor;
use huitai::quota::{QuotaReader, quota};

#[test]
#[ignore = "读本机真实数据、会联网，手动跑"]
fn print_live_readings() {
    for vendor in Vendor::ALL {
        let reader = QuotaReader::system().expect("取不到 HOME");
        let started = Instant::now();
        let reports = reader.probe(vendor);
        println!("{vendor} 全部渠道试一遍用时 {:?}", started.elapsed());
        for report in reports {
            println!("{vendor} · {}: {:?}", report.source, report.result);
        }
        let started = Instant::now();
        let reading = quota(vendor);
        println!(
            "{vendor} 最终读数（{:?}）: {reading:?}\n",
            started.elapsed()
        );
    }
}

/// 打印给会话台启动的 Claude 用的 `--settings` 值，项目目录取环境变量 `HUITAI_CWD`（默认当前目录）。
#[test]
#[ignore = "手动核对状态栏注入"]
fn print_claude_settings() {
    let cwd = std::env::var_os("HUITAI_CWD")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap());
    println!(
        "{}",
        huitai::quota::claude_settings(&cwd).expect("取不到 HOME")
    );
}
