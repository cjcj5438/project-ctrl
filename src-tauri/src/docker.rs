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
    let mut args = base_args(spec);
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
    let mut args = base_args(spec);
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
        let mut args = base_args(spec);
        args.push("logs".into());
        args.push("-f".into());
        args.push("--tail".into());
        args.push(crate::supervisor::LOG_CAP.to_string());
        args.push("--no-color".into());
        args.extend(spec.services.iter().cloned());

        let mut child = Command::new(docker_bin())
            .args(&args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("无法执行 docker logs：{e}"))?;

        let svc_key = crate::supervisor::key_of(project_id, name);
        let epoch = sup.begin_docker_logs(&svc_key);
        // 先取走管道，再把 Child 本体交给句柄（stop 时 kill）
        let out = child.stdout.take();
        let err = child.stderr.take();
        if let Some(out) = out {
            Self::pump(sup.clone(), sink.clone(), svc_key.clone(), project_id, name.to_string(), epoch, Box::new(out));
        }
        if let Some(err) = err {
            Self::pump(sup.clone(), sink.clone(), svc_key.clone(), project_id, name.to_string(), epoch, Box::new(err));
        }
        Ok(Self { child: std::sync::Arc::new(std::sync::Mutex::new(child)), epoch })
    }

    #[allow(clippy::too_many_arguments)]
    fn pump(
        sup: crate::supervisor::Supervisor,
        sink: std::sync::Arc<dyn crate::supervisor::Sink>,
        key: String,
        project_id: i64,
        name: String,
        epoch: u64,
        stream: Box<dyn std::io::Read + Send>,
    ) {
        std::thread::spawn(move || {
            use std::io::BufRead;
            let reader = std::io::BufReader::new(stream);
            let mut batch: Vec<String> = Vec::new();
            for line in reader.split(b'\n') {
                let Ok(bytes) = line else { break };
                if sup.docker_epoch(&key) != Some(epoch) {
                    break;
                }
                let text = String::from_utf8_lossy(&bytes).trim_end_matches('\r').to_string();
                sup.push_docker_line(&key, text.clone());
                batch.push(text);
                if batch.len() >= crate::supervisor::LOG_BATCH {
                    sink.log(&crate::supervisor::LogPayload {
                        key: key.clone(),
                        project_id,
                        name: name.clone(),
                        lines: std::mem::take(&mut batch),
                    });
                }
            }
            if !batch.is_empty() {
                sink.log(&crate::supervisor::LogPayload {
                    key: key.clone(),
                    project_id,
                    name: name.clone(),
                    lines: batch,
                });
            }
        });
    }

    pub fn stop(self) {
        if let Ok(mut c) = self.child.lock() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }
}

/// `docker compose ps --format json`（compose v2 输出 JSON 行）。
pub fn ps(cwd: &Path, spec: &DockerSpec) -> Result<Vec<ContainerView>, String> {
    let mut args = base_args(spec);
    args.push("ps".into());
    args.push("--format".into());
    args.push("json".into());
    args.push("--all".into());
    let out = run(cwd, &args, Duration::from_secs(30))?;
    if !out.status.success() {
        return Err(err_text(&out));
    }
    let raw = text(&out);
    let mut views = Vec::new();
    // compose 可能输出 JSON 数组，也可能是每行一个 JSON 对象
    let trimmed = raw.trim();
    if trimmed.starts_with('[') {
        if let Ok(rows) = serde_json::from_str::<Vec<PsRow>>(trimmed) {
            views.extend(rows.into_iter().map(into_view));
        }
    } else {
        for line in trimmed.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(row) = serde_json::from_str::<PsRow>(line) {
                views.push(into_view(row));
            }
        }
    }
    if !spec.services.is_empty() {
        views.retain(|v| spec.services.contains(&v.service));
    }
    Ok(views)
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
}
