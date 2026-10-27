//! docker compose 编排：只当命令执行器 + `ps` 状态读取者，不重写 compose 语义。
//!
//! 为什么不做「容器级接管」：compose 的 profiles / extends / env 插值 / 依赖图永远追不完，
//! 让 docker 自己当 supervisor，我们只读它的 JSON 状态、转发它的日志。

use crate::services::DockerSpec;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

/// 单条 `docker compose ps` 的容器视图。
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct ContainerView {
    pub name: String,
    pub service: String,
    pub state: String,
    pub status: String,
    pub ports: String,
}

/// compose 的 `ps --format json` 用 PascalCase 字段名，这里显式映射到 snake_case。
#[derive(Deserialize)]
struct PsRow {
    #[serde(default, rename = "Name")]
    name: String,
    #[serde(default, rename = "Service")]
    service: String,
    #[serde(default, rename = "State")]
    state: String,
    #[serde(default, rename = "Status")]
    status: String,
    #[serde(default, rename = "Publishers")]
    publishers: Vec<Publisher>,
}

#[derive(Deserialize)]
struct Publisher {
    #[serde(default, rename = "PublishedPort")]
    published_port: u16,
}

/// 组装 `docker compose` 的公共前缀参数（多 `-f` 按声明顺序叠加）。
/// 只剩测试用：产品路径统一走 `checked_base_args`（#18 终检）。
#[cfg(test)]
pub fn base_args(spec: &DockerSpec) -> Vec<String> {
    let mut args: Vec<String> = vec!["compose".into()];
    for f in &spec.files {
        args.push("-f".into());
        args.push(f.clone());
    }
    if let Some(p) = &spec.project_name {
        args.push("-p".into());
        args.push(p.clone());
    }
    args
}

/// 带越界终检的参数组装（#18）：每个 `-f` 都经 canonicalize 确认仍在项目内。
/// `up/down/ps/follow` 的 cwd 即 `Resolved.base`，与 `-f` 的相对路径同基准。
fn checked_base_args(cwd: &Path, spec: &DockerSpec) -> Result<Vec<String>, String> {
    let mut args: Vec<String> = vec!["compose".into()];
    for f in &spec.files {
        let p = crate::services::resolve_within(cwd, f)?;
        args.push("-f".into());
        args.push(p.display().to_string());
    }
    if let Some(p) = &spec.project_name {
        args.push("-p".into());
        args.push(p.clone());
    }
    Ok(args)
}

fn docker_bin() -> String {
    // 优先用 PATH 里的 docker（Docker Desktop / colima 都可能）
    crate::execenv::resolve("docker", &crate::execenv::base_path())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "docker".to_string())
}

fn run(cwd: &Path, args: &[String], timeout: Duration) -> Result<std::process::Output, String> {
    let mut child = Command::new(docker_bin())
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("无法执行 docker：{e}"))?;
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() > timeout => {
                let _ = child.kill();
                // #51：kill 后必须 wait 收尸，否则僵尸进程残留。
                let _ = child.wait();
                return Err(format!("docker 超时（{}s）", timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(format!("等待 docker 失败：{e}")),
        }
    }
    child.wait_with_output().map_err(|e| format!("读取 docker 输出失败：{e}"))
}

/// `docker compose up -d [services...]`。
pub fn up(cwd: &Path, spec: &DockerSpec) -> Result<String, String> {
    let mut args = checked_base_args(cwd, spec)?;
    args.push("up".into());
    args.push("-d".into());
    args.extend(spec.services.iter().cloned());
    let out = run(cwd, &args, Duration::from_secs(180))?;
    if !out.status.success() {
        return Err(err_text(&out));
    }
    Ok(text(&out))
}

/// `docker compose down`（只针对声明的文件组合，不删卷）。
pub fn down(cwd: &Path, spec: &DockerSpec) -> Result<String, String> {
    let mut args = checked_base_args(cwd, spec)?;
    args.push("down".into());
    if !spec.services.is_empty() {
        // `down` 不接受服务名；用 stop 表达「只停这几个」
        args.pop();
        args.push("stop".into());
        args.extend(spec.services.iter().cloned());
    }
    let out = run(cwd, &args, Duration::from_secs(120))?;
    if !out.status.success() {
        return Err(err_text(&out));
    }
    Ok(text(&out))
}

/// `docker compose logs -f` 的流式读取器。
///
/// 复用 supervisor 的 `Sink`（状态 + 日志事件），所以 compose 的输出和普通服务一样
/// 出现在同一个日志面板里，前端不需要第二套通道。
///
/// 返回的句柄被 drop 时不会自动杀进程——由 [`DockerLogs::stop`] 显式停止，
/// 与「停止服务」的语义保持一致。
pub struct DockerLogs {
    child: std::sync::Arc<std::sync::Mutex<std::process::Child>>,
    /// 本次流的代号：旧流的残留行不会写进新一次运行的日志。
    epoch: u64,
}

impl DockerLogs {
    /// 开始跟随日志。`svc_key` 用与普通服务相同的 `"{project_id}/{name}"`。
    pub fn follow(
        sup: &crate::supervisor::Supervisor,
        sink: std::sync::Arc<dyn crate::supervisor::Sink>,
        cwd: &Path,
        spec: &DockerSpec,
        project_id: i64,
        name: &str,
    ) -> Result<Self, String> {
        Self::follow_with_bin(sup, sink, cwd, spec, project_id, name, docker_bin())
    }

    /// 同 [`follow`]，但可指定 docker 可执行文件（测试用假脚本模拟两层进程）。
    #[allow(clippy::too_many_arguments)]
    pub fn follow_with_bin(
        sup: &crate::supervisor::Supervisor,
        sink: std::sync::Arc<dyn crate::supervisor::Sink>,
        cwd: &Path,
        spec: &DockerSpec,
        project_id: i64,
        name: &str,
        bin: String,
    ) -> Result<Self, String> {
        let mut args = checked_base_args(cwd, spec)?;
        args.push("logs".into());
        args.push("-f".into());
        args.push("--tail".into());
        args.push(crate::supervisor::LOG_CAP.to_string());
        args.push("--no-color".into());
        args.extend(spec.services.iter().cloned());

        let mut cmd = Command::new(bin);
        cmd.args(&args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        {
            // 独立进程组：`docker compose` 会再 spawn `cli-plugins/docker-compose`，
            // 只 kill 第一层会留下插件子进程继续跟随日志。
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let mut child = cmd.spawn().map_err(|e| format!("无法执行 docker logs：{e}"))?;
        let svc_key = crate::supervisor::key_of(project_id, name);
        let epoch = sup.begin_docker_logs(&svc_key);
        // 先取走管道，再把 Child 本体交给句柄（stop 时 kill）
        let out = child.stdout.take();
        let err = child.stderr.take();
        // 两路读线程只管往 pending 里攒，另起一个冲刷线程按时间 emit：
        // `logs -f` 吐完历史后会阻塞等新行（不 EOF），攒满 N 行才推会让输出少
        // 的容器第一批永远到不了 UI。语义与受管服务的 spawn_flusher 一致。
        let pending: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let readers = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        if let Some(out) = out {
            readers.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Self::pump(
                sup.clone(),
                svc_key.clone(),
                epoch,
                Box::new(out),
                pending.clone(),
                readers.clone(),
            );
        }
        if let Some(err) = err {
            readers.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Self::pump(
                sup.clone(),
                svc_key.clone(),
                epoch,
                Box::new(err),
                pending.clone(),
                readers.clone(),
            );
        }
        Self::spawn_flusher(
            sup.clone(),
            sink,
            svc_key,
            project_id,
            name.to_string(),
            epoch,
            pending,
            readers,
        );
        Ok(Self { child: std::sync::Arc::new(std::sync::Mutex::new(child)), epoch })
    }

    /// 读线程：逐行进环形缓冲，并交给冲刷线程攒批（不在这里 emit）。
    fn pump(
        sup: crate::supervisor::Supervisor,
        key: String,
        epoch: u64,
        stream: Box<dyn std::io::Read + Send>,
        pending: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        readers: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) {
        std::thread::spawn(move || {
            use std::io::BufRead;
            let reader = std::io::BufReader::new(stream);
            for line in reader.split(b'\n') {
                let Ok(bytes) = line else { break };
                if sup.docker_epoch(&key) != Some(epoch) {
                    break;
                }
                let text = String::from_utf8_lossy(&bytes).trim_end_matches('\r').to_string();
                sup.push_docker_line(&key, text.clone());
                let mut g = pending.lock().unwrap_or_else(|e| e.into_inner());
                // 上限是内存保险：极长的构建输出不应该在冲刷线程睡觉时无限堆积
                if g.len() < crate::supervisor::LOG_BATCH * 10 {
                    g.push(text);
                }
            }
            readers.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        });
    }

    /// 每 `LOG_FLUSH` 把攒下的行推一次；两路读线程都结束后再推最后一批后退出。
    ///
    /// 若代号已前进（新流顶掉了本流）：直接丢弃尾批退出——旧行的归宿是旧面板，
    /// 推给新流会污染它的首屏。
    #[allow(clippy::too_many_arguments)]
    fn spawn_flusher(
        sup: crate::supervisor::Supervisor,
        sink: std::sync::Arc<dyn crate::supervisor::Sink>,
        key: String,
        project_id: i64,
        name: String,
        epoch: u64,
        pending: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        readers: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) {
        std::thread::spawn(move || loop {
            std::thread::sleep(crate::supervisor::LOG_FLUSH);
            if sup.docker_epoch(&key) != Some(epoch) {
                return;
            }
            let take = |pending: &std::sync::Arc<std::sync::Mutex<Vec<String>>>| -> Vec<String> {
                let mut g = pending.lock().unwrap_or_else(|e| e.into_inner());
                std::mem::take(&mut *g)
            };
            let batch = take(&pending);
            if !batch.is_empty() {
                sink.log(&crate::supervisor::LogPayload {
                    key: key.clone(),
                    project_id,
                    name: name.clone(),
                    lines: batch,
                });
            }
            if readers.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                let tail = take(&pending);
                if !tail.is_empty() {
                    sink.log(&crate::supervisor::LogPayload { key, project_id, name, lines: tail });
                }
                return;
            }
        });
    }

    /// 停掉整条日志跟随：先 TERM 进程组，短暂等待，仍未退出则 KILL。
    ///
    /// 必须按**进程组**停：`docker compose` 的行为主体在插件子进程里，
    /// 只杀父进程会让插件继续跟随日志（实测留下 PPID=1 的孤儿）。
    pub fn stop(self) {
        let Ok(mut c) = self.child.lock() else { return };
        let pgid = c.id() as i32;
        crate::supervisor::reclaim_group_graceful(pgid);
        let _ = c.wait();
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// 跟随进程的 pid（测试断言进程存活/回收用）。
    #[cfg(test)]
    pub fn pid(&self) -> u32 {
        self.child.lock().map(|c| c.id()).unwrap_or(0)
    }

    /// 跟随进程是否仍在跑（供退出清理的回归测试断言）。
    #[cfg(test)]
    pub fn is_alive(&self) -> bool {
        let Ok(mut c) = self.child.lock() else { return false };
        matches!(c.try_wait(), Ok(None))
    }
}

/// `docker compose ps --format json`（compose v2 输出 JSON 行）。
pub fn ps(cwd: &Path, spec: &DockerSpec) -> Result<Vec<ContainerView>, String> {
    let mut args = checked_base_args(cwd, spec)?;
    args.push("ps".into());
    args.push("--format".into());
    args.push("json".into());
    args.push("--all".into());
    let out = run(cwd, &args, Duration::from_secs(30))?;
    if !out.status.success() {
        return Err(err_text(&out));
    }
    let views = parse_ps_output(&out)?;
    if spec.services.is_empty() {
        return Ok(views);
    }
    Ok(views.into_iter().filter(|v| spec.services.contains(&v.service)).collect())
}

/// 解析 `docker compose ps --format json` 的输出（#27）。
///
/// 只读 stdout（stderr 的 WARN 不得污染 JSON）；任一行解析失败 → 整个 ps 返回
/// Err（带行号与原文摘要），不再静默吞成空列表。
fn parse_ps_output(out: &std::process::Output) -> Result<Vec<ContainerView>, String> {
    let raw = String::from_utf8_lossy(&out.stdout);
    let mut views = Vec::new();
    // compose 可能输出 JSON 数组，也可能是每行一个 JSON 对象
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(views);
    }
    if trimmed.starts_with('[') {
        let rows: Vec<PsRow> = serde_json::from_str(trimmed).map_err(|e| {
            format!("docker ps 输出解析失败：{e}（原文摘要：{}）", summarize(trimmed))
        })?;
        views.extend(rows.into_iter().map(into_view));
    } else {
        for (idx, line) in trimmed.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let row: PsRow = serde_json::from_str(line).map_err(|e| {
                format!("docker ps 第 {} 行解析失败：{e}（原文：{}）", idx + 1, summarize(line))
            })?;
            views.push(into_view(row));
        }
    }
    Ok(views)
}

/// 原文摘要：截断到 120 字符，避免把超长输出塞进错误信息。
fn summarize(s: &str) -> String {
    const LIMIT: usize = 120;
    if s.len() <= LIMIT {
        return s.to_string();
    }
    let mut end = LIMIT;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

fn into_view(row: PsRow) -> ContainerView {
    let ports = row
        .publishers
        .iter()
        .filter(|p| p.published_port > 0)
        .map(|p| p.published_port.to_string())
        .collect::<Vec<_>>()
        .join(",");
    ContainerView {
        name: row.name,
        service: row.service,
        state: row.state,
        status: row.status,
        ports,
    }
}

fn text(out: &std::process::Output) -> String {
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    let e = String::from_utf8_lossy(&out.stderr);
    if !e.trim().is_empty() {
        if !s.is_empty() && !s.ends_with('\n') {
            s.push('\n');
        }
        s.push_str(&e);
    }
    s
}

fn err_text(out: &std::process::Output) -> String {
    let t = text(out);
    let t = t.trim();
    if t.is_empty() {
        format!("docker 执行失败（退出码 {:?}）", out.status.code())
    } else {
        t.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> DockerSpec {
        DockerSpec {
            files: vec!["infra/docker-compose.yml".into(), "docker-compose.gpu.yml".into()],
            project_name: Some("g6".into()),
            services: vec!["postgres".into(), "minio".into()],
        }
    }

    #[test]
    fn base_args_keeps_file_order_and_project_name() {
        let args = base_args(&spec());
        assert_eq!(
            args,
            vec!["compose", "-f", "infra/docker-compose.yml", "-f", "docker-compose.gpu.yml", "-p", "g6"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn down_degrades_to_stop_when_services_listed() {
        // 直接断言参数组合：down 不接受服务名，所以声明了子服务时改用 stop
        let s = spec();
        let mut args = base_args(&s);
        args.push("down".into());
        if !s.services.is_empty() {
            args.pop();
            args.push("stop".into());
            args.extend(s.services.iter().cloned());
        }
        assert_eq!(args.last().unwrap(), "minio");
        assert!(args.contains(&"stop".to_string()));
        assert!(!args.contains(&"down".to_string()));
    }

    #[test]
    fn ps_rows_parse_json_array_and_lines() {
        let arr = r#"[{"Name":"g6-postgres-1","Service":"postgres","State":"running","Status":"Up 2h","Publishers":[{"PublishedPort":5432}]}]"#;
        let rows: Vec<PsRow> = serde_json::from_str(arr).unwrap();
        let v = into_view(rows.into_iter().next().unwrap());
        assert_eq!(v.service, "postgres");
        assert_eq!(v.ports, "5432");

        let lines = "{\"Name\":\"a\",\"Service\":\"web\",\"State\":\"exited\",\"Status\":\"Exited (0)\",\"Publishers\":[]}";
        let rows: Vec<PsRow> = lines.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(into_view(rows.into_iter().next().unwrap()).state, "exited");
    }

    #[test]
    fn ps_parses_stdout_only_and_fails_loudly() {
        use std::os::unix::process::ExitStatusExt;
        use std::process::Output;
        let ok_row = "{\"Name\":\"g6-postgres-1\",\"Service\":\"postgres\",\"State\":\"running\",\"Status\":\"Up\",\"Publishers\":[]}";
        // stderr 有 WARN 也不得污染解析
        let out = Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: format!("{ok_row}\n").into_bytes(),
            stderr: b"WARN: using default tag\n".to_vec(),
        };
        let views = parse_ps_output(&out).unwrap();
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].service, "postgres");
        // stdout 非 JSON → 整个 ps 返回 Err（带行号/原文摘要），不再吞成空列表
        let bad = Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: format!("{ok_row}\nNOT-JSON{{{{\n").into_bytes(),
            stderr: Vec::new(),
        };
        let err = parse_ps_output(&bad).unwrap_err();
        assert!(err.contains('2'), "{err}");
        assert!(err.contains("NOT-JSON"), "{err}");
    }

    #[test]
    fn compose_file_symlink_escape_is_rejected() {
        // #18：docker.files 经链接指向仓库外 → up/ps/follow 之前先拒绝。
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("evil.yml"), "services: {}\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            outside.path().join("evil.yml"),
            dir.path().join("compose.yml"),
        )
        .unwrap();
        let spec = DockerSpec {
            files: vec!["compose.yml".into()],
            project_name: None,
            services: vec![],
        };
        let err = checked_base_args(dir.path(), &spec).unwrap_err();
        assert!(err.contains("越界"), "{err}");
        // 项目内正常文件放行，且解析为绝对路径（docker 不再看到相对穿越写法）。
        std::fs::write(dir.path().join("ok.yml"), "services: {}\n").unwrap();
        let spec = DockerSpec { files: vec!["ok.yml".into()], project_name: None, services: vec![] };
        let args = checked_base_args(dir.path(), &spec).unwrap();
        assert!(args.iter().any(|a| a.ends_with("ok.yml") && a.starts_with('/')), "{args:?}");
    }
}
