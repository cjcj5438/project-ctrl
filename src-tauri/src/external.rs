//! 外部监督者的只读视图：PM2 的 `ecosystem.config.js` 与 launchd 的 plist。
//!
//! 为什么只读：这两套已经在你的机器上跑着，它们有自己的保活策略。App 若去写 plist
//! 或调 `pm2 start`，就会和它们的 KeepAlive 抢同一个进程——那是「点了启动，结果两边
//! 打架」这类最难查的故障。这里只做三件事：读出来、显示状态、标出与已管理内容的差异。

use serde::Serialize;
use std::path::{Path, PathBuf};

/// 一条外部条目。
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct ExternalEntry {
    /// `pm2` | `launchd`
    pub source: String,
    /// 展示名（PM2 的 app name / plist 的 Label）。
    pub name: String,
    /// 工作目录（若外部配置里写了）。
    pub cwd: Option<String>,
    /// 该条目是否指向一个已在本 App 里管理的项目。
    pub managed_project: Option<String>,
    /// 该目录在本 App 里是否已有同名服务声明。
    pub declared: bool,
    /// 人读的状态说明（`launchd: loaded` / `pm2: 配置里的 8 个 app`）。
    pub note: String,
    /// 原始配置文件的路径（供「在 Finder 中显示」）。
    pub config_path: String,
    /// 该条目的命令（PM2 的 script+args / launchd 的 ProgramArguments）。
    pub command: Option<Vec<String>>,
    /// 工作目录（`cwd`）的修改时间，用于判断「配置文件是否已变」。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changed_at: Option<i64>,
}

/// 扫 PM2 的 ecosystem 配置。`preferred` 是用户常用的位置，找不到就返回空。
pub fn pm2_entries(home: &Path) -> Vec<ExternalEntry> {
    let candidates = [
        home.join("my/PM2/ecosystem.config.js"),
        home.join("PM2/ecosystem.config.js"),
        home.join("ecosystem.config.js"),
    ];
    let Some(path) = candidates.iter().find(|p| p.is_file()) else { return Vec::new() };
    pm2_entries_from(path)
}

/// 解析单个 ecosystem 文件（用 node 求值，配置是 JS 不是 JSON）。
pub fn pm2_entries_from(path: &Path) -> Vec<ExternalEntry> {
    let path_s = path.to_string_lossy().into_owned();
    let Some(node) = crate::execenv::resolve("node", &crate::execenv::base_path()) else { return Vec::new() };
    let script = "const c=require(process.argv[1]);const a=c.apps||[];process.stdout.write(JSON.stringify(a.map(x=>({name:x.name,cwd:x.cwd||null,script:x.script||null,args:x.args||null}))))";
    let out = match std::process::Command::new(node).args(["-e", script, &path_s]).output() {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    #[derive(serde::Deserialize)]
    struct App {
        name: String,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        script: Option<String>,
        #[serde(default)]
        args: Option<String>,
    }
    let mtime = file_mtime(path);
    let apps: Vec<App> = serde_json::from_slice(&out.stdout).unwrap_or_default();
    apps.into_iter()
        .map(|a| {
            let mut command = a.script.clone().map(|s| vec![s]);
            if let (Some(cmd), Some(args)) = (command.as_mut(), a.args.as_ref()) {
                cmd.extend(split_args(args));
            }
            ExternalEntry {
                source: "pm2".into(),
                note: if a.cwd.as_deref().map(|c| Path::new(c).is_dir()).unwrap_or(false) {
                    "配置存在".into()
                } else {
                    "工作目录不存在".into()
                },
                name: a.name,
                cwd: a.cwd,
                managed_project: None,
                declared: false,
                config_path: path_s.clone(),
                command,
                changed_at: mtime,
            }
        })
        .collect()
}

/// 扫 `~/Library/LaunchAgents/*.plist`：只读 Label / ProgramArguments / WorkingDirectory。
///
/// 不调用 `launchctl`（那会 fork 一个子进程且输出格式随系统变），只做 plist 文本解析。
pub fn launchd_entries(home: &Path) -> Vec<ExternalEntry> {
    let dir = home.join("Library/LaunchAgents");
    let Ok(rd) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut out = Vec::new();
    for e in rd.filter_map(|e| e.ok()) {
        let path = e.path();
        if path.extension().map(|x| x != "plist").unwrap_or(true) {
            continue;
        }
        let path_s = path.to_string_lossy().into_owned();
        // #57：二进制 plist 先用 plutil 转文本；彻底损坏的记一条可解释的条目而不是丢弃。
        let text = match plist_text(&path) {
            Ok(t) => t,
            Err(note) => {
                out.push(ExternalEntry {
                    source: "launchd".into(),
                    name: path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
                    cwd: None,
                    managed_project: None,
                    declared: false,
                    note,
                    config_path: path_s,
                    command: None,
                    changed_at: file_mtime(&path),
                });
                continue;
            }
        };
        let label = plist_string(&text, "Label").unwrap_or_else(|| {
            path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
        });
        let cwd = plist_string(&text, "WorkingDirectory");
        let argv = plist_array(&text, "ProgramArguments");
        let program = argv.first().cloned();
        let keep_alive = text.contains("<key>KeepAlive</key>");
        let mut note_bits: Vec<String> = Vec::new();
        if keep_alive {
            note_bits.push("KeepAlive".into());
        }
        if let Some(p) = &program {
            note_bits.push(p.clone());
        }
        out.push(ExternalEntry {
            source: "launchd".into(),
            name: label,
            cwd,
            managed_project: None,
            declared: false,
            note: if note_bits.is_empty() { "已安装".into() } else { note_bits.join(" · ") },
            config_path: path.to_string_lossy().into_owned(),
            command: if argv.is_empty() { None } else { Some(argv) },
            changed_at: file_mtime(&path),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// 读 plist 文本：UTF-8 文本直接读；二进制则用 `plutil -convert xml1 -o -` 转文本（#57）。
/// 都失败 → 返回 note 文案，调用方记一条可解释的条目而不是丢弃。
fn plist_text(path: &Path) -> Result<String, String> {
    let raw = std::fs::read(path).map_err(|_| "无法解析的 plist（二进制/损坏）".to_string())?;
    // 二进制 plist 以 `bplist` 魔数开头：必须经 plutil 转文本，直接当 UTF-8 读会误判。
    if raw.starts_with(b"bplist") {
        let out = std::process::Command::new("/usr/bin/plutil")
            .args(["-convert", "xml1", "-o", "-", &path.to_string_lossy()])
            .output();
        return match out {
            Ok(o) if o.status.success() => String::from_utf8(o.stdout)
                .map_err(|_| "无法解析的 plist（二进制/损坏）".to_string()),
            _ => Err("无法解析的 plist（二进制/损坏）".to_string()),
        };
    }
    // 文本形态：必须含 plist 结构标记，否则是损坏文件（不能按"已安装"显示）。
    let text = String::from_utf8(raw).map_err(|_| "无法解析的 plist（二进制/损坏）".to_string())?;
    if text.contains("<plist") || text.contains("<dict") || text.contains("<key>") {
        Ok(text)
    } else {
        Err("无法解析的 plist（二进制/损坏）".to_string())
    }
}

/// 切分 PM2 `args` 字符串（#79）：支持单/双引号与反斜杠转义。
///
/// 规则：空白分隔；`"…"`/`'…'` 内空白保留；`\x` 转义为字面 `x`（含 `\"`、`\'`、`\ `）；
/// 未闭合的引号按到串尾结束处理（不报错，尽量还原用户意图）。
pub(crate) fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut in_word = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                cur.push(next);
                in_word = true;
            }
            continue;
        }
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else {
                cur.push(c);
            }
            continue;
        }
        match c {
            '"' | '\'' => {
                quote = Some(c);
                in_word = true;
            }
            _ if c.is_whitespace() => {
                if in_word {
                    out.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            _ => {
                cur.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        out.push(cur);
    }
    out
}

/// 从 plist 文本里取某个 key 的 `<string>` 值（够用即可，不引入 XML 依赖）。
fn plist_string(text: &str, key: &str) -> Option<String> {
    let needle = format!("<key>{key}</key>");
    let at = text.find(&needle)?;
    let rest = &text[at + needle.len()..];
    let open = rest.find("<string>")?;
    let after = &rest[open + "<string>".len()..];
    let close = after.find("</string>")?;
    Some(after[..close].trim().to_string())
}

/// 取 `<array>` 里的全部 `<string>`。
fn plist_array(text: &str, key: &str) -> Vec<String> {
    let needle = format!("<key>{key}</key>");
    let Some(at) = text.find(&needle) else { return Vec::new() };
    let rest = &text[at + needle.len()..];
    let Some(open) = rest.find("<array>") else { return Vec::new() };
    let after_open = &rest[open + "<array>".len()..];
    let Some(close) = after_open.find("</array>") else { return Vec::new() };
    let body = &after_open[..close];
    let mut out = Vec::new();
    let mut idx = 0;
    while let Some(s) = body[idx..].find("<string>") {
        let start = idx + s + "<string>".len();
        let Some(e) = body[start..].find("</string>") else { break };
        out.push(body[start..start + e].trim().to_string());
        idx = start + e + "</string>".len();
    }
    out
}

/// 文件修改时间的 unix 秒（判断「配置是否已变」用）。
fn file_mtime(path: &Path) -> Option<i64> {
    let meta = std::fs::metadata(path).ok()?;
    let t = meta.modified().ok()?;
    t.duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_secs() as i64)
}

/// 把外部条目与已管理项目对齐：把 cwd 归一化后比对项目路径。
pub fn annotate(entries: &mut [ExternalEntry], projects: &[(String, Vec<String>)]) {
    for e in entries.iter_mut() {
        let Some(cwd) = e.cwd.as_deref() else { continue };
        let cwd = crate::logic::normalize_path(cwd);
        if let Some((path, names)) = projects.iter().find(|(p, _)| *p == cwd) {
            e.managed_project = Some(path.clone());
            e.declared = names.contains(&e.name);
        }
    }
}

/// 把「外部命令」与「本 App 里的同名声明」比对，得出漂移结论。
///
/// 这是「同步」的核心：配置文件被改过、或声明里的命令与外部不一致时，
/// UI 必须能说出来——否则用户会以为两边是同一份。
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Drift {
    pub kind: &'static str,
    pub detail: String,
}

pub fn drift_of(entry: &ExternalEntry, declared_cmd: Option<&[String]>) -> Option<Drift> {
    let declared = declared_cmd?;
    let external = entry.command.as_deref().unwrap_or(&[]);
    if external.is_empty() || declared.is_empty() {
        return None;
    }
    if external == declared {
        return None;
    }
    Some(Drift {
        kind: "command-mismatch",
        detail: format!("外部：{} ／ 声明：{}", external.join(" "), declared.join(" ")),
    })
}

/// 汇总：`~/Library/LaunchAgents` + PM2 配置。
pub fn scan(home: &Path) -> Vec<ExternalEntry> {
    let mut all = launchd_entries(home);
    all.extend(pm2_entries(home));
    all
}

/// 供 UI 用的默认 home。
pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_fields_are_extracted() {
        let text = r#"<?xml version="1.0"?>
<plist version="1.0"><dict>
  <key>Label</key><string>com.chenjing.omp-hub</string>
  <key>ProgramArguments</key>
  <array><string>/Users/x/hub</string><string>--port</string><string>18790</string></array>
  <key>WorkingDirectory</key><string>/Users/x/my/omp-mobile</string>
  <key>KeepAlive</key><true/>
</dict></plist>"#;
        assert_eq!(plist_string(text, "Label").as_deref(), Some("com.chenjing.omp-hub"));
        assert_eq!(
            plist_string(text, "WorkingDirectory").as_deref(),
            Some("/Users/x/my/omp-mobile")
        );
        assert_eq!(plist_array(text, "ProgramArguments").first().map(String::as_str), Some("/Users/x/hub"));
    }

    #[test]
    fn missing_key_is_none() {
        assert_eq!(plist_string("<dict></dict>", "Label"), None);
    }

    #[test]
    fn launchd_dir_is_scanned_read_only() {
        let d = tempfile::tempdir().unwrap();
        let agents = d.path().join("Library/LaunchAgents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(
            agents.join("a.plist"),
            "<plist><dict><key>Label</key><string>ai.test.demo</string><key>WorkingDirectory</key><string>/tmp</string></dict></plist>",
        )
        .unwrap();
        std::fs::write(agents.join("ignored.txt"), "nope").unwrap();
        let entries = launchd_entries(d.path());
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0].name, "ai.test.demo");
        assert_eq!(entries[0].source, "launchd");
        assert_eq!(entries[0].cwd.as_deref(), Some("/tmp"));
    }

    #[test]
    fn annotate_marks_managed_and_declared() {
        let mut entries = vec![ExternalEntry {
            source: "launchd".into(),
            name: "ai.test.demo".into(),
            cwd: Some("/x/proj/".into()),
            managed_project: None,
            declared: false,
            note: String::new(),
            config_path: "/x/a.plist".into(),
            command: None,
            changed_at: None,
        }];
        annotate(&mut entries, &[("/x/proj".to_string(), vec!["api".to_string()])]);
        assert_eq!(entries[0].managed_project.as_deref(), Some("/x/proj"));
        assert!(!entries[0].declared, "名字不同就不算已声明");

        annotate(&mut entries, &[("/x/proj".to_string(), vec!["ai.test.demo".to_string()])]);
        assert!(entries[0].declared);
    }

    #[test]
    fn plist_array_reads_all_arguments() {
        let text = r#"<key>ProgramArguments</key>
<array><string>/usr/bin/node</string><string>hub.js</string><string>--port</string><string>18790</string></array>"#;
        assert_eq!(
            plist_array(text, "ProgramArguments"),
            vec!["/usr/bin/node", "hub.js", "--port", "18790"]
        );
    }

    #[test]
    fn drift_reports_command_mismatch_only() {
        let mut e = ExternalEntry {
            source: "launchd".into(),
            name: "ai.test".into(),
            cwd: None,
            managed_project: None,
            declared: true,
            note: String::new(),
            config_path: "/x.plist".into(),
            command: Some(vec!["node".into(), "hub.js".into()]),
            changed_at: None,
        };
        assert!(drift_of(&e, None).is_none(), "没声明就没有可比的东西");
        assert!(
            drift_of(&e, Some(&["node".to_string(), "hub.js".to_string()])).is_none(),
            "一致时不该报漂移"
        );
        let d = drift_of(&e, Some(&["node".to_string(), "server.js".to_string()])).unwrap();
        assert_eq!(d.kind, "command-mismatch");
        assert!(d.detail.contains("hub.js") && d.detail.contains("server.js"), "{}", d.detail);
        e.command = None;
        assert!(drift_of(&e, Some(&["node".into()])).is_none(), "外部没有命令时不做判断");
    }

    #[test]
    fn missing_pm2_and_launchd_yield_empty_not_error() {
        let d = tempfile::tempdir().unwrap();
        assert!(scan(d.path()).is_empty());
    }

    #[test]
    fn binary_plist_is_converted_not_skipped() {
        let d = tempfile::tempdir().unwrap();
        let agents = d.path().join("Library/LaunchAgents");
        std::fs::create_dir_all(&agents).unwrap();
        let plist = agents.join("bin.plist");
        std::fs::write(
            &plist,
            "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>Label</key><string>ai.test.binary</string><key>ProgramArguments</key><array><string>/bin/echo</string></array></dict></plist>",
        )
        .unwrap();
        // 现场转二进制（plutil 是 macOS 系统工具，测试可用）
        let st = std::process::Command::new("/usr/bin/plutil")
            .args(["-convert", "binary1", &plist.to_string_lossy()])
            .status()
            .unwrap();
        assert!(st.success());
        assert!(std::fs::read(&plist).unwrap().starts_with(b"bplist"), "应确为二进制 plist");
        // 二进制 → plutil 转回文本 → 条目仍在
        let entries = launchd_entries(d.path());
        assert!(entries.iter().any(|e| e.name == "ai.test.binary"), "{entries:?}");
        // 彻底损坏的 plist → 记一条可解释的条目而不是丢弃
        std::fs::write(&plist, b"\x00\x01\x02garbage-not-plist".as_slice()).unwrap();
        let entries = launchd_entries(d.path());
        let e = entries.iter().find(|e| e.config_path == plist.to_string_lossy()).expect("损坏的 plist 也要有条目");
        assert!(e.note.contains("无法解析"), "{}", e.note);
    }

    #[test]
    fn split_args_handles_quotes_and_escapes() {
        assert!(split_args("").is_empty());
        assert_eq!(split_args("--port 8080"), vec!["--port", "8080"]);
        assert_eq!(split_args("--msg \"hello world\""), vec!["--msg", "hello world"]);
        assert_eq!(split_args("--msg 'it works'"), vec!["--msg", "it works"]);
        assert_eq!(split_args("--path a\\ b"), vec!["--path", "a b"]);
        assert_eq!(split_args("\"a\\\"b\""), vec!["a\"b"]);
        assert_eq!(split_args("a'b'c"), vec!["abc"]);
    }

    #[test]
    fn drift_of_empty_declared_is_none() {
        // #41 补点：drift_reports_command_mismatch_only 已覆盖一致→None、mismatch+detail、
        // 外部空命令→None；这里只补它没断言的「声明为空数组 → None」。
        let e = ExternalEntry {
            source: "launchd".into(),
            name: "ai.test".into(),
            cwd: None,
            managed_project: None,
            declared: true,
            note: String::new(),
            config_path: "/x.plist".into(),
            command: Some(vec!["node".into(), "hub.js".into()]),
            changed_at: None,
        };
        assert!(drift_of(&e, Some(&[])).is_none(), "声明为空数组时不做判断");
    }
}
