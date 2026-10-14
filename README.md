# project-ctrl

本机自用的 macOS 桌面项目管理器：收集散落各目录的项目，归类（tag），一键进入。

里程碑 1：脚手架 + SQLite + 添加项目（扫描并勾选 / 添加单个）+ 列表页 +
tag（手动 tag 打/摘/改名/删除、自动 tag、筛选、搜索、排序）+ 路径丢失标记及重新关联/移除。

里程碑 2：卡片打开按钮——Finder / iTerm（新标签页并 cd 到项目）/ VS Code /
GitHub（仅识别出 GitHub remote 时显示，用系统默认浏览器打开仓库页）。
命令只接收项目 id，路径与 GitHub URL 均由后端从数据库与 `.git/config` 现算，
外部程序一律以参数数组调用（不拼 shell 字符串）；iTerm 的 AppleScript 用
`on run argv` + `quoted form of`，路径含空格/中文/引号/`$`/反引号也安全。
`src-tauri/Info.plist` 提供 `NSAppleEventsUsageDescription`（iTerm 自动化授权说明）。

里程碑 3：详情页 + README。点卡片进详情（返回后保留搜索/筛选/排序）；详情含 tag 增删、
四个打开按钮、「从管理器移除」。README 支持渲染 / 编辑 / 生成：Markdown 经 marked 转
HTML 后用 DOMPurify 清洗再插入（拦截 script/iframe/on* 事件/javascript: 链接），
相对路径图片经 `read_project_image` 校验（限项目目录内、扩展名与 10MB 白名单）后转 data URL；
CodeMirror 6 左编辑右实时预览，Cmd+S 保存并按纳秒 mtime 检测外部冲突（冲突弹框，
不静默覆盖）；无 README 时用 `create_new` 生成模板（绝不覆盖）。安全：`app.security.csp`
禁止内联/外部脚本，写入 README 前 canonicalize 校验位于项目目录内（拒绝符号链接写穿）。
后端只收项目 id，路径由数据库取；对用户目录只写 README 本身。

里程碑 4：详情页只读 git。项目存在 `.git`（目录或文件，含 worktree/submodule）时，在打开按钮旁
增加「git status」「git log」按钮（由后端 `has_git` 与自动 tag `git` 共用同一个 `git_kind_of` 判定）；
点击后在详情页内以等宽、保留换行、可滚动可复制的面板显示输出，可切换/关闭，失败或超时在面板里
显示 stderr/「超时」说明。`run_git(id, kind)` 的 kind 只接受白名单 `status|log`（不接受任意子命令），
用 `Command` 参数数组、`current_dir(项目)`、`-c color.ui=false -c core.quotepath=false`（无 ANSI、中文不转义）、
`GIT_OPTIONAL_LOCKS=0`/`GIT_TERMINAL_PROMPT=0`/`GIT_PAGER=cat`（只读、不写 index、不弹终端、不分页）；
30 秒超时会真正 kill 并回收子进程，stdout/stderr 独立线程读取防管道写满，输出 >1MB 截断并附提示；
git 程序优先用绝对路径 `/usr/bin/git`（macOS GUI PATH 精简），不存在则回退 `git`。

## 环境

- pnpm 11、node 22、rust 1.99、Xcode CLT
- `@tauri-apps/cli` 为项目 devDependency（不要全局安装）

## 命令

```sh
pnpm install
pnpm tauri dev     # 开发运行
pnpm build         # 前端类型检查 + 构建
cargo test         # Rust 单元测试（在 src-tauri/ 下运行）
```

数据目录默认 `~/.project-ctrl/`，可用环境变量 `PROJECT_CTRL_DATA_DIR` 覆盖
（测试/验证时指向 /tmp 临时目录，避免污染真实数据）。

## 约束

- 对用户项目目录只写 README.md（里程碑 3 才涉及），绝不在项目目录创建其他文件；
  git 全程只读，绝不 commit/add/pull/push/fetch。
- 不要 `git push`。
