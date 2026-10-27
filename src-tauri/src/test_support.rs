//! 测试间的共享前置：会占用真实端口的测试必须串行，否则会互相「顺延」到对方的端口，
//! 使 `wait_listening` 观察到错误的端口而产生偶发失败。

#![cfg(test)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

/// 任何会 spawn 监听进程 / 占端口的测试都要持有它。
static PORT_LOCK: Mutex<()> = Mutex::new(());

pub fn exclusive_ports() -> MutexGuard<'static, ()> {
    PORT_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// `PROJECT_CTRL_DATA_DIR` 是**进程级**环境变量：任何改它的测试都必须串行，
/// 否则并行测试会读到对方的库（表现为「凭据莫名消失」这类偶发失败）。
static DATA_DIR_LOCK: Mutex<()> = Mutex::new(());

/// 把数据目录指到 `dir`，并在离开作用域时恢复原值。
pub struct ScopedDataDir {
    _guard: MutexGuard<'static, ()>,
    prev: Option<OsString>,
}

impl ScopedDataDir {
    pub fn new(dir: &Path) -> Self {
        let guard = DATA_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var_os("PROJECT_CTRL_DATA_DIR");
        let mut p = PathBuf::from(dir);
        p.push(""); // 保持目录形态，避免末尾斜杠差异
        std::env::set_var("PROJECT_CTRL_DATA_DIR", dir);
        Self { _guard: guard, prev }
    }
}

impl Drop for ScopedDataDir {
    fn drop(&mut self) {
        match self.prev.take() {
            Some(v) => std::env::set_var("PROJECT_CTRL_DATA_DIR", v),
            None => std::env::remove_var("PROJECT_CTRL_DATA_DIR"),
        }
    }
}
