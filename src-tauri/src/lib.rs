mod commands;
#[cfg(test)]
mod e2e_tests;
#[cfg(test)]
mod ipc_tests;
mod db;
mod descriptor;
mod detect;
mod docker;
mod execenv;
mod external;
mod git;
mod localstate;
mod logic;
mod plan;
mod ports;
mod readme;
mod services;
mod size;
mod source;
#[cfg(test)]
mod test_support;
mod supervisor;

/// 插件、全局状态、命令表。`run` 与测试共用同一份，保证测试跑的是真实装配。
fn configure<R: tauri::Runtime>(builder: tauri::Builder<R>) -> tauri::Builder<R> {
    builder
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .manage(supervisor::Supervisor::default())
        .manage(commands::DockerLogsRegistry::default())
        .invoke_handler(tauri::generate_handler![
            commands::list_projects,
            commands::list_tags,
            commands::dev_pick_dir,
            commands::scan_directory,
            commands::add_projects,
            commands::set_project_tags,
            commands::rename_tag,
            commands::delete_tag,
            commands::relink_project,
            commands::remove_project,
            commands::open_in_finder,
            commands::open_in_vscode,
            commands::open_in_iterm,
            commands::open_github,
            commands::read_readme,
            commands::save_readme,
            commands::generate_readme,
            commands::read_project_image,
            commands::open_external,
            commands::run_git,
            commands::project_size,
            commands::project_services,
            commands::save_service,
            commands::delete_service,
            commands::solidify_descriptor,
            commands::set_service_override,
            commands::service_override,
            commands::read_descriptor,
            commands::service_status,
            commands::service_logs,
            commands::service_logs_since,
            commands::service_preflight,
            commands::service_start,
            commands::service_stop,
            commands::service_stop_all,
            commands::service_step,
            commands::open_service_url,
            commands::docker_up,
            commands::docker_down,
            commands::docker_ps,
            commands::docker_logs,
            commands::docker_logs_stop,
            commands::probe_all,
            commands::adopt_candidates,
            commands::run_adhoc,
            commands::external_scan,
            commands::reveal_file,
            commands::source_info,
            commands::source_fetch,
            commands::source_pull,
            commands::clone_repo,
            commands::import_pm2,
        ])
}

/// 生产装配。菜单保持启用（macOS 应用菜单是发布形态的一部分）。
pub fn builder<R: tauri::Runtime>() -> tauri::Builder<R> {
    configure(tauri::Builder::<R>::new())
}

/// 测试装配：用 Tauri 的 mock runtime（关掉 macOS 菜单——菜单对象只能在主线程创建），
/// 其余与生产完全一致（同一份 `tauri.conf.json`）。
///
/// `generate_context!` 每个 crate 只能用一次（它会定义一个符号），所以放在这里共享。
#[cfg(test)]
pub fn test_app() -> tauri::App<tauri::test::MockRuntime> {
    configure(tauri::test::mock_builder())
        .build(tauri::generate_context!())
        .expect("mock app 应能构建")
}

/// 信号 handler 往里写一个字节的管道写端。handler 内只能做 async-signal-safe
/// 调用，所以只写 fd，真正的退出逻辑在 `signal-exit` 线程里。
#[cfg(not(test))]
static SIGNAL_PIPE_FD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

/// 信号入口：只写一个字节进管道就返回（async-signal-safe）。
#[cfg(not(test))]
unsafe extern "C" fn on_signal(sig: libc::c_int) {
    // SAFETY：只调 async-signal-safe 的 write；fd 在 main 线程装好且进程生命期内不关闭。
    unsafe {
        let b = sig as u8;
        let fd = SIGNAL_PIPE_FD.load(std::sync::atomic::Ordering::Relaxed);
        if fd >= 0 {
            libc::write(fd, &b as *const u8 as *const libc::c_void, 1);
        }
    }
}

/// 加上信号钩子的主动退出，不加守卫会被重复执行（重复 stop 无害但浪费并刷日志）。
#[cfg(not(test))]
static EXIT_CLEANUP_DONE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 两条退出清理：① compose 日志流 ② 受管服务。返回 true 表示本次真正执行了。
#[cfg(not(test))]
fn exit_cleanup<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> bool {
    use std::sync::atomic::Ordering;
    use tauri::Manager;
    if EXIT_CLEANUP_DONE.swap(true, Ordering::SeqCst) {
        return false;
    }
    // ① compose 日志跟随是独立子进程：父进程退出后它会被 launchd 收养继续跑，
    //    所以必须先显式停掉，否则每退出一次就漏一个 `docker compose logs -f`。
    if let Some(reg) = app.try_state::<commands::DockerLogsRegistry>() {
        reg.stop_all();
    }
    // ② 受管服务进程：保留 keep_alive 的，收掉其余的。
    if let Some(sup) = app.try_state::<supervisor::Supervisor>() {
        commands::register_adoptable(&sup);
    }
    true
}

/// 生产入口。测试构建里不编译它——`generate_context!` 会定义一个全局符号，
/// 一个 crate 只能出现一次，测试装配必须独占它。
#[cfg(not(test))]
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = builder::<tauri::Wry>()
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    // 启动时先收养上次留下的进程（声明了 keep_alive 的服务），再进入事件循环。
    {
        use tauri::Manager;
        if app.try_state::<supervisor::Supervisor>().is_some() {
            commands::adopt_existing(app.handle());
        }
    }

    // SIGTERM/SIGINT/SIGHUP：默认行为是直接死，Exit 事件钩子跑不到，
    // `docker compose logs -f` 与受管服务会变孤儿（实测 pkill 后 PPID=1）。
    // 这里把信号翻译成一次正常的 app.exit(0)，走同一套退出清理。
    {
        let handle = app.handle().clone();
        use std::os::fd::FromRawFd;
        let mut fds = [0; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            eprintln!("信号管道创建失败，SIGTERM 下可能残留子进程");
        } else {
            // SAFETY：fds 由 pipe 刚创建，所有权移交给 File，不再重复关闭。
            let read = unsafe { std::fs::File::from_raw_fd(fds[0]) };
            SIGNAL_PIPE_FD.store(fds[1], std::sync::atomic::Ordering::Relaxed);
            // SAFETY：函数指针与 sighandler_t 同宽，as 指针转换是标准写法；
            // signal(2) 返回 SIG_ERR 时打印错误。
            let handler = on_signal as *const () as libc::sighandler_t;
            for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
                if unsafe { libc::signal(sig, handler) } == libc::SIG_ERR {
                    eprintln!("信号 {sig} 监听失败，SIGTERM 下可能残留子进程");
                }
            }
            std::thread::Builder::new()
                .name("signal-exit".into())
                .spawn(move || {
                    use std::io::Read;
                    let mut read = read;
                    let mut buf = [0u8; 1];
                    // 读到任一信号字节就正常退出（exit_cleanup 有一次性守卫）。
                    if read.read(&mut buf).is_ok() {
                        handle.exit(0);
                    }
                })
                .expect("信号监听线程应能启动");
        }
    }

    app.run(|app, event| {
        // 退出路径有两条：macOS 的 Cmd+Q 只发 `Exit`，不经过 `ExitRequested`（实测）。
        // 两条都要收子进程，否则关掉 App 后服务继续裸跑、占着端口。
        if matches!(event, tauri::RunEvent::ExitRequested { .. } | tauri::RunEvent::Exit) {
            exit_cleanup(app);
        }
    });
}
