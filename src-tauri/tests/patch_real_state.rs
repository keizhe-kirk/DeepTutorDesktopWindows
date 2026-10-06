//! 真机状态回归：对着**真实用户目录**跑一遍补丁判据。
//!
//! 这条测试存在的理由：判据曾经用「播种那一刻碰巧在跑的版本」当基线，
//! 而该字段只在 `None` 时才写 —— 于是后端一热更新就永久失效，且**没有任何
//! 症状**（后端正常起、菜单报「补丁未就绪」、重启多少次都没用）。
//! 纯临时目录的单元测试看不出这类"跨版本生命周期"的故障，
//! 必须拿真实 `state.json` 钉住。
//!
//! 忽略运行：
//! ```bash
//! cargo test --manifest-path src-tauri/Cargo.toml --test patch_real_state \
//!   -- --ignored --nocapture
//! ```

use deeptutor_shell_lib::backend::patch::Patches;

#[test]
#[ignore]
fn real_user_state_is_applicable_for_the_active_version() {
    let home = std::env::var("LOCALAPPDATA").expect("LOCALAPPDATA");
    let patches = Patches::detect(Some(std::path::PathBuf::from(home).join("DeepTutor")));

    let state = patches.state();
    println!("root        = {}", patches.root().display());
    println!("enabled     = {}", state.is_enabled());
    println!("initialized = {}", state.initialized);
    println!(
        "for_bundled = {:?}  (纯诊断,不参与判定)",
        state.for_bundled
    );
    println!(
        "manifest    = {:?}",
        patches.manifest().map(|m| m.verified_deeptutor)
    );

    // 真机当前状态：内置 1.6.12 / 叠加层 active 1.6.13。
    for v in ["1.6.12", "1.6.13"] {
        let ok = patches.is_applicable(Some(v));
        println!("applicable({v}) = {ok}");
        assert!(ok, "生效版 {v} 应当适用 —— 修复前这里会因旧基线而 false");
        assert!(patches.effective_dir(Some(v)).is_some());
    }

    // 未验证的版本仍必须被拦住（白名单不是摆设）。
    assert!(
        !patches.is_applicable(Some("9.9.9")),
        "清单外的版本必须停用补丁"
    );
}