# project-ctrl

本机自用的 macOS 桌面项目管理器：把散落在各目录里的项目收集起来、打 tag 归类，
一键用 Finder / iTerm / VS Code 打开，并查看 README 与只读的 git 状态。

## 功能

- **添加项目**：「扫描文件夹…」选择目录后列出其一级子目录（跳过隐藏目录，已加入的不再出现），
  勾选后可批量打 tag 再「加入」；「添加单个项目…」把选中的目录本身作为项目加入。候选项目不要求有 `.git`。
- **列表页**：卡片网格（项目名＝文件夹名、路径、创建日期、tag）；顶部名称搜索、tag 多选筛选（AND 语义）、
  排序切换（创建时间倒序 / 名称）。手动 tag 可在筛选栏改名 / 删除（删除前确认，并从所有项目摘掉）。
- **自动 tag**（不入库、不可手改，也可用于筛选）：`git`（存在 `.git`）、`无git`、`github`（`.git` 为目录且 remote 含 github.com）。
- **详情页**：点卡片进入（返回后保留搜索 / 筛选 / 排序）。可增删手动 tag；Finder / iTerm / VS Code /
  （识别出 GitHub remote 时）GitHub 按钮；「从管理器移除」只删记录、不动文件。
- **README**：渲染（marked 转 HTML 后经 DOMPurify 清洗）、编辑（CodeMirror 6，左编辑右实时预览，
  `Cmd+S` 保存；编辑期间文件被外部改动会提示冲突，不静默覆盖）、无 README 时按模板生成。
- **只读 git**：存在 `.git` 的项目可查看 `git status` / `git log`，输出显示在详情页面板里（等宽、可滚动、可复制）。
- **项目大小**：卡片与详情页显示项目磁盘占用（如 `12.3 MB`）。目录统计在后台线程进行（并发上限 3），
  列表不会被阻塞；结果只缓存在前端内存（不入库），路径丢失的项目不统计也不显示。
- **路径丢失**（目录被删 / 改名 / 移动）：保留记录，卡片与详情页显示「路径丢失」，提供「重新关联」与「移除」。
- **源码更新**（详情页「源码」面板）：显示分支 / upstream / 未提交改动数 / 领先 / 落后；「检查更新」跑
  `git fetch`，「拉取」跑 `git pull --ff-only`（仅在干净、有 upstream、无未推送提交、确实落后时可用，
  否则按钮置灰并写明原因）。
- **服务管理**（详情页「服务」面板）：为项目保存一份「安装 / 构建 / 启动」配方（argv 数组，不经过 shell），
  一键启动受管进程、看实时日志、停止 / 重启、按端口打开浏览器。首次使用会按锁文件与技术栈探测出候选启动命令。
- **端口不冲突**：启动前探测配置的开发端口，被占则顺延（最多 +9），并把「改用了哪个端口、原来的端口被谁占」写进状态；
  spawn 后还会实测实际监听端口回填。依赖端口（如容器里的 pg）只检查是否已监听，只警告不阻塞。
- **preflight**：按与执行完全相同的方式解析 PATH，缺可执行文件（如某项目要 `go` 而本机没装）时按钮置灰并说明缺什么，
  不会「点得动但必然失败」。
- **从 GitHub 克隆**：粘贴 https / ssh 地址 + 选父目录 → `git clone` → 自动加入管理器。
- **导入 PM2 配置**：读 `~/my/PM2/ecosystem.config.js`，把其中的应用转成项目 + 配方（已存在的不覆盖，cwd 不存在的跳过）。

## 使用文档

带截图的完整使用说明见 [`docs/USAGE.md`](docs/USAGE.md)。

## 环境

- macOS（Apple Silicon）、pnpm 11、node 22、Rust stable、Xcode Command Line Tools
- `@tauri-apps/cli` 是项目 devDependency（不要全局安装）

## 开发

```sh
pnpm install
pnpm tauri dev      # 开发运行（同时启动 vite 与 Tauri 应用）
pnpm build          # 前端类型检查 + 构建
cargo test          # Rust 单元测试（在 src-tauri/ 下运行）
```

## 构建 release .app

```sh
pnpm tauri build --bundles app
```

产物在 `src-tauri/target/release/bundle/macos/project-ctrl.app`（arm64，ad-hoc 签名），复制到
`/Applications` 即可使用。`tauri.conf.json` 中 `bundle.macOS.signingIdentity = "-"`：Apple Silicon 上
未签名二进制无法运行，这里用 ad-hoc 签名，使 `pnpm tauri build` 一步产出可运行的 .app。
不做公证、不做自动更新、不生成 dmg。

本地构建出的 .app 不带 quarantine 标记，通常可直接打开；本项目未做公证，若 .app 是从别处拷贝或下载来的
而遇到「无法验证开发者」，可在「系统设置 → 隐私与安全性」中对该应用选择「仍要打开」。

## 发布与更新

### 发版流程

1. 同步版本号：`package.json` 与 `src-tauri/Cargo.toml` 都改成新版本（两者必须一致）。
2. commit，然后打 tag 并推送：
   ```sh
   git tag vX.Y.Z
   git push origin vX.Y.Z
   ```
3. tag 会触发 `.github/workflows/release.yml`（`macos-latest`，arm64）：CI 先校验 tag（去掉 `v`）与
   上述两处版本号一致，再跑 `pnpm build` 与 `cargo test`，最后用 tauri-action 构建并创建 Release，
   附带 `project-ctrl.app.tar.gz`、其 `.sig` 与 `latest.json`（App 的更新源）。

### GitHub Secrets

- `TAURI_SIGNING_PRIVATE_KEY`：更新签名私钥的**内容**（即 `~/.tauri/project-ctrl.key` 的文件内容）。
  该密钥无密码，因此不需要 `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`。
- **私钥务必自行备份。** 私钥丢失后，已安装的 App 将无法收到更新（签名校验不通过），只能手动重新安装。

### App 内更新行为

- 启动后**静默检查一次**更新源（Release 的 `latest.json`）；检查失败（断网 / 404 / 限流 / 尚未发布）一律忽略，不提示。
- 有新版本时窗口顶部出现横幅「发现新版本 vX.Y.Z」+ 更新说明（纯文本，过长自动折叠），按钮「立即更新」「稍后」。
- 只有点「立即更新」才下载并安装（按钮上显示下载进度百分比），完成后自动重启；安装失败的错误显示在横幅里。
  「稍后」只关闭横幅，本次会话不再提示。不做定时检查，也不做「跳过此版本」。

### 安装脚本

```sh
scripts/install.sh                 # 从最新 Release 安装 / 升级到 /Applications/project-ctrl.app
scripts/install.sh --tag v0.1.0    # 指定 tag
scripts/install.sh --dry-run       # 只打印将执行的步骤
scripts/install.sh --open          # 安装完成后启动
```

依赖已登录的 `gh`（私有 / 公开仓库都可用）。若应用正在运行会先请求退出（最多等约 10 秒），仍在运行则报错退出，
**不会强制结束进程**；安装后清除 `com.apple.quarantine`。

### ad-hoc 签名的影响

本项目不做 Apple 开发者证书与公证，仅 ad-hoc 签名（`signingIdentity: "-"`），因此每次更新后 .app 的代码指纹都会变化；
推测量化权限（例如「控制 iTerm」的自动化授权）可能在更新后需要重新确认——**此点为推测，未实测**。

## 数据与配置

- SQLite 库位于 `~/.project-ctrl/`，4 张表：`projects`（路径唯一、加入时间）、`tags`（名称唯一）、
  `project_tags`（关联，外键级联）、`services`（每个项目至多一份配方 JSON，外键级联）。
- 环境变量 `PROJECT_CTRL_DATA_DIR` 可覆盖数据目录（测试 / 验证时指向 `/tmp`，避免污染真实数据）。

## 对用户目录的写入边界

- 只写项目目录里的 README 文件本身（生成 / 保存；不创建临时文件或备份文件）。
- git 默认只执行 `status` 与 `log` 两个只读命令（环境带 `GIT_OPTIONAL_LOCKS=0`，不改写仓库索引）。
  新增的**源码更新**是显式按钮，且有前置条件：
  - `git fetch`（只写 `.git` 里的 remote 引用，不改工作区）随时可用；
  - `git pull --ff-only` 仅当「有 upstream + 工作区干净 + 无未推送提交 + 落后 > 0」时可用，
    否则按钮置灰并显示原因；
  - `git clone` 只在你指定父目录、且目标目录不存在时执行。
  **不提供** `reset --hard` / `stash` / `checkout` / `push`，任何可能丢弃工作区改动的操作都不做。
- 受管服务的子进程环境从零构造（`env_clear()` 后只设 PATH / HOME / TERM / LANG / NO_COLOR 与配方自报的
  `env`），**不继承 shell 环境**，因此 `.zshrc` 里的密钥不会被带进服务进程。
- 打开外部链接只允许 `http` / `https` / `mailto`。

## 服务运行器的工作方式

- 配方只存在库里（`services.config`），不往项目目录写任何配置文件。
- 子进程用 `process_group(0)` 起在独立进程组里，停止时对整组发 `TERM`（宽限 5 秒）再 `KILL`，
  因此 `pnpm dev` 派生出的 node/vite 不会逃出去继续占端口。
- 子进程环境从零构造（`env_clear()` 后只给 `PATH`/`HOME`/`TERM`/`LANG`/`NO_COLOR` 与配方自报的变量），
  不继承 shell 环境，所以 `.zshrc` 里的密钥不会进入服务进程。
- 日志按服务保留最近 2000 行，攒批推送（不逐行），前端订阅事件增量追加。
- 运行状态只存在于内存：App 重启后一律从「未运行」开始，不收养上次残留的进程。
- 关闭 App（包括 macOS 的 `Cmd+Q`，它只发 `RunEvent::Exit`）会收掉全部**仍在运行/构建**的进程组；
  已停止的记录不会再被发信号（进程组早已退出，pid 可能已被系统复用给别的进程）。

## 已知局限

- 创建日期读取目录 birthtime，目录被拷贝或备份恢复后会变。
- `.git` 为文件时（linked worktree / submodule）只标记 `git`，不做 GitHub 识别。
- 首次点击 iTerm 按钮时，macOS 会询问是否允许 project-ctrl 控制 iTerm（自动化授权）。
- 开发端口被占时只顺延「框架能吃下端口参数」的情形（`--port` / `-p` / `PORT`）；项目里硬编码的端口
  （如 `VITE_VIEWER_URL=http://127.0.0.1:5173`）不会被改写，状态里会明确提示这一点。
- 端口探测除 bind 外还查一次 `lsof`：本机 docker 只在 IPv6 `*:8080` 上监听，而 macOS 仍允许绑
  `127.0.0.1:8080`，只看 bind 会把已被占用的端口误判为空闲。
- 项目大小口径为**磁盘占用**（`st_blocks × 512`，与 `du` 同口径）：递归包含 `node_modules` 与 `.git`，
  同一 inode 的硬链接只算一次，不跟随符号链接，读不了的子目录跳过；显示用 1000 进制
  （B / KB / MB / GB / TB，小于 1000 B 显示整数 B，其余保留 1 位小数）。
  APFS 上目录自身在 `st_blocks` 中报 0，因此在极端的海量硬链接场景下与 `du` 的统计可能略有出入。

## 开发提示

调试（debug）构建支持用数据目录下的 `dev-pick-dir` 文件旁路原生选目录弹窗，便于自动化验证；
release 构建恒不生效。
