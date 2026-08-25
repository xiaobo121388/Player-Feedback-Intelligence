mod api;
mod app;
mod crypto;
mod error;
mod mcp;
mod models;
mod session;

use std::{
    io::{self, Write},
    sync::Arc,
};

use api::Service;

fn main() {
    let is_mcp = std::env::args().any(|argument| argument == "--mcp");
    let is_headless_login = std::env::args().any(|argument| argument == "--headless-login");
    let is_headless_cookie_login =
        std::env::args().any(|argument| argument == "--headless-cookie-login");
    if !is_mcp && !is_headless_login && !is_headless_cookie_login {
        hide_console_window();
    }

    let service = match Service::new() {
        Ok(service) => Arc::new(service),
        Err(error) => {
            eprintln!("{}", error.message);
            std::process::exit(1);
        }
    };

    if is_mcp || is_headless_login || is_headless_cookie_login {
        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                eprintln!("MCP 运行时启动失败：{error}");
                std::process::exit(1);
            }
        };
        if is_headless_login {
            if let Err(error) = runtime.block_on(run_headless_login(service)) {
                eprintln!("登录失败：{error}");
                std::process::exit(1);
            }
            return;
        }
        if is_headless_cookie_login {
            if let Err(error) = runtime.block_on(run_headless_cookie_login(service)) {
                eprintln!("Cookie 登录失败：{error}");
                std::process::exit(1);
            }
            return;
        }
        if let Err(error) = runtime.block_on(mcp::run(service)) {
            eprintln!("MCP 服务异常退出：{error}");
            std::process::exit(1);
        }
        return;
    }

    if let Err(error) = app::run(service) {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

async fn run_headless_login(service: Arc<Service>) -> Result<(), String> {
    eprint!("网易开发者账号：");
    io::stderr()
        .flush()
        .map_err(|error| format!("无法读取账号：{error}"))?;

    let mut account = String::new();
    io::stdin()
        .read_line(&mut account)
        .map_err(|error| format!("无法读取账号：{error}"))?;
    let password = rpassword::prompt_password("密码（不会显示）：")
        .map_err(|error| format!("无法读取密码：{error}"))?;

    let outcome = service
        .login_password(account, password)
        .await
        .map_err(|error| error.message)?;

    report_persisted_login(outcome)
}

async fn run_headless_cookie_login(service: Arc<Service>) -> Result<(), String> {
    let cookie = rpassword::prompt_password("NTES_SESS（不会显示）：")
        .map_err(|error| format!("无法读取 Cookie：{error}"))?;
    let outcome = service
        .login_cookie(cookie)
        .await
        .map_err(|error| error.message)?;

    report_persisted_login(outcome)
}

fn report_persisted_login(outcome: crate::models::LoginOutcome) -> Result<(), String> {
    if !outcome.persisted {
        return Err(outcome
            .warning
            .unwrap_or_else(|| "系统钥匙串不可用，会话未持久保存".to_string()));
    }

    let nickname = outcome
        .account
        .nickname
        .as_deref()
        .unwrap_or("未命名开发者");
    eprintln!("登录成功，会话已保存：{nickname}");
    Ok(())
}

#[cfg(all(windows, not(debug_assertions)))]
fn hide_console_window() {
    use windows_sys::Win32::{
        System::Console::{GetConsoleProcessList, GetConsoleWindow},
        UI::WindowsAndMessaging::{SW_HIDE, ShowWindow},
    };

    // SAFETY: these are read-only/window-visibility Win32 APIs. Hide only a console that
    // belongs solely to this process; never hide a terminal inherited from PowerShell/cmd.
    unsafe {
        let mut processes = [0u32; 2];
        if GetConsoleProcessList(processes.as_mut_ptr(), processes.len() as u32) != 1 {
            return;
        }
        let window = GetConsoleWindow();
        if !window.is_null() {
            ShowWindow(window, SW_HIDE);
        }
    }
}

#[cfg(any(not(windows), debug_assertions))]
fn hide_console_window() {}
