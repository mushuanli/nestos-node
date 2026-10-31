# nestos-node — NestOS 的个人智能节点

nestos-node 将设备上的文件、项目和执行环境接入 NestOS，统一提供同步、受控访问及 AI harness 会话管理。NestOS 组织知识、对话和工作流，nestos-node 连接设备上的数据与执行能力。

服务源码：[nestos-node](https://github.com/mushuanli/nestos-node)；统一客户端：[nestos-node-driver](https://github.com/mushuanli/nestos-node-driver)，npm 包为 `@itookit/nestos-node-driver`。

服务端目前支持 Linux（需要 `openat2`）；Web、Tauri 和 Node CLI 使用同一 HTTP 协议。服务启动时验证目录句柄能力，不支持时拒绝启动。

节点功能已接入统一插件框架，可通过 `[plugins]` 分别开关文件接口、命令执行、同步、项目、Harness 和 MCP。未配置新开关时沿用原有行为。接口、装配顺序和独立验证见 [节点插件](doc/plugins.md)，配置见 [插件示例](config.plugins.example.toml)；可执行 `nestos-node plugins check CONFIG [PLUGIN]` 校验全部或单个插件而不启动服务。

```bash
# 在本项目根目录执行（itookit monorepo 内即 tools/nestos-node，作为 submodule）
cargo build --release
# 复制 config.example.toml，设置实际目录、监听地址和允许的 Web/Tauri Origin。
export NESTOS_NODE_API_KEY='替换为至少24字节的API-Key'
target/release/nestos-node                     # 自动读取 config.toml
target/release/nestos-node /path/to/config.toml # 也可显式指定
```

文件模式配置一组认证凭据和一串导出目录；可额外启用外部 harness 控制：

```toml
listen = "127.0.0.1:8787"
allowed_origins = ["http://localhost:3000", "http://localhost:1420"]

api_key_env = "NESTOS_NODE_API_KEY"      # 也可内联 api_key = "至少24字节"

[[exports]]
path = "/n/prj/x1"                   # 别名默认取目录名：x1

[[exports]]
path = "/srv/projects/demo"
access = "rw"                        # 默认 ro；rw 自动加独占锁
```

`allowed_origins` 是 CORS 白名单，只决定「哪个网页的 JS 能读取响应」，不代替认证：`http://localhost:3000` 对应 Web 开发（`apps/web-app` 的 Vite 端口），`http://localhost:1420` 对应 Tauri 开发；打包后的 Tauri 用 `tauri://localhost`（Linux/macOS）或 `http://tauri.localhost`（Windows），生产 Web 填实际 https 域名。写 `["*"]` 放通所有源（仅限本机/受控环境），写 `[]` 则拒绝所有浏览器源；两种写法都不影响 CLI 与 curl。

配置文件统一使用 TOML。不传参数时，服务依次在**进程当前工作目录**和**可执行文件所在目录**查找 `config.toml`，取第一个存在的文件；显式参数优先。找不到配置文件时启动失败并列出已搜索路径。

默认只读。需要写入时，在对应 `[[exports]]` 增加 `access = "rw"`（自动请求独占锁）。工作台添加挂载时再勾选允许写入；三层权限都允许才能修改。

启动时逐项显示实际打开的导出别名与 `ro/rw`、`read/write` 状态，`server.ready` 事件的 `exports` 数组提供相同信息，写权限同时核对目录能力和当前认证身份。此状态表示服务端导出权限；已注册项目及 NestOS 保存的客户端绑定还可限制为只读，修改导出配置不会自动提升已有项目权限。

已有项目可通过 `project_remount(projectId, revision, access)` 修改根权限，保留 ID、目录和原生会话。权限变更校验导出授权、原目录身份和版本，拒绝在途执行；提交后关闭旧项目 harness 与 watcher，使旧 revision 失效。读写根仍受额外挂载各自的授权限制，不修改导出配置。响应丢失时先读取项目核对结果。

### 文件 metadata 与目录挂载

旧 version-1 元数据库可能没有 `identity` 列。读取时兼容该布局且不改库；首次 metadata 写入在事务中补列并分配稳定身份，保留原有 metadata。迁移记录 `metadata.schema.upgraded`，SQLite 失败记录 `metadata.database.failed` 的安全错误类别，不记录正文。

可写目录注册为项目时，节点在实际项目根创建 `.nestos/meta.sqlite`，子目录项目也使用自己的存储边界。
注册、配置、重新挂载及启动恢复均维护项目名称和身份；注册信息按远端主体隔离，不代替授权目录。
已有 `.mindos/` 整目录改名保留文件；父范围中的 metadata 转入项目范围，文件稳定身份不变。
未注册项目的文件沿用 export 根存储，普通读取不创建数据库；只读项目不会创建目录。
同范围重命名维护记录；跨范围移动明确拒绝，避免遗失 metadata。取消注册保留项目 metadata。
`ro` 可以读取已有 metadata，但不能写入或创建数据库；`rw` 仍必须同时通过 export、认证身份与项目挂载授权。
这项能力不需要新增 TOML 开关，也不是只针对 AI Chat 的私有存储。

数据库 key 使用 **export 根内的规范来源相对路径**。例如实际文件 `/srv/work/docs/a.md`、export 根 `/srv/work`，
key 始终是 `docs/a.md`；客户端挂载 `/srv/work` 或其 `docs` 子目录、使用不同挂载别名，不会改变这个 key。
服务器先做项目路径授权及防穿越校验，再访问同一份远程 metadata，不能提交绝对宿主路径或跨项目查询。
通过节点执行的重命名/移动会维护子树的 metadata 路径，删除清理对应记录；正文替换保留同路径 metadata。
如果文件操作已完成但 metadata 维护失败，回执明确标记 `partial`；请核对远程实际路径和 metadata 后修复，不要把它当作“未发生”重发。
metadata 修改不会改变正文 revision，也不会使原本合法的正文条件保存失效。

`.nestos` 是保留目录，不经文件 API、列表或普通同步暴露；不能通过公开文件 API编辑其中的 SQLite。
迁移或备份项目时请同时保留 `.nestos`，不要当作临时缓存删除；停服务后一起复制，或使用 SQLite 的一致性备份方式。
仅复制正文不会迁移 AI 关联等自定义 metadata。不要为了同一棵目录的不同挂载层级重复配置重叠 export，
也不要手工编辑数据库或删除其 journal 文件。旧节点不声明 metadata 能力时，客户端明确拒绝写入，不在本地另存替代记录。

`exclusive` 要求所有修改经本服务完成；目录上的 advisory lock 阻止合作的同根实例，不能阻止编辑器、Git 或其他外部程序。配置内拒绝重叠根与重复别名；不同实例的父子导出根仍需部署侧禁止。共享修改目录使用 `ro`。服务支持 HTTP；公网部署在 TLS 反向代理之后。Origin 白名单只解决浏览器访问，不代替身份认证。用户名/密码使用 UTF-8 HTTP Basic，密码至少 8 字节：用户名取 `username`，未写时读环境变量 `FS_SERVER_USER`；密码取内联 `password` 或 `password_env` 指向的环境变量。Basic 凭据不加密，非可信本机网络应使用 HTTPS。推荐 `api_key` 或 `api_key_env`（至少 24 字节、不设置 `username`），使用 Bearer 认证；旧名称 `token`/`token_env` 仍兼容，但不能和对应新名称同时配置。API Key 与密码认证互斥；同时设置 `username` 与 `token`/`token_env` 会启动失败，避免看起来是 Basic 实际只收 Bearer。内联 secret 写在服务端配置文件中。API Key 按启动连接提示输出到 console，不放入结构化事件或 HTTP 响应；旧 Basic 密码不输出。

在“工具箱 → MCP”新增 HTTP 配置，填写完整 MCP 地址（例如 `http://127.0.0.1:8787/mcp`），将服务启动时输出的 API Key 填入同名字段并测试连接。通用发现响应已携带 nestos-node 能力描述；应用扩展验证后启用项目绑定，不需要额外用户名、密码或勾选框。旧服务器使用其已声明的能力工具。测试后的保存和改名复用验证结果，普通 MCP 不发 nestos-node 专用探测。只有验证成功的 nestos-node 配置可以用于目录绑定，普通 MCP 即使同名也不能绑定。可配置多条连接；工作台新建远程项目选择 MCP 配置和 `/alias/path`。项目展示“服务器名:项目名”，同一服务器、账号与规范化路径复用已有项目，改名不移动目录。

连接配置、API Key 和项目引用通过现有 MCP 配置持久保存；应用重启时从已验证的 MCP 配置恢复驱动凭据，无需重新输入。旧 Basic 配置的密码仍只保存在宿主运行期。CLI 持久配置引用由 `NESTOS_REMOTE_<credentialRef中横线替换为下划线>` 环境变量解析。直接 `nestos fs` 使用 `FS_SERVER_USER` + `FS_SERVER_PASSWORD`；未设置用户名时兼容 `FS_SERVER_TOKEN`。

```bash
# 使用已构建的 CLI；也可以通过现有 CLI 开发入口运行。
node apps/cli/dist/cli.js fs list http://127.0.0.1:8787 docs
node apps/cli/dist/cli.js fs read http://127.0.0.1:8787 docs guide.md --out /tmp/guide.md
node apps/cli/dist/cli.js fs stat http://127.0.0.1:8787 docs guide.md
node apps/cli/dist/cli.js fs status http://127.0.0.1:8787 docs <operationId>
```

API：`GET /v1/exports`、`POST /v1/fs/:alias/stat`（`{paths}`）、`GET entries?path=`、`GET/PUT content?path=`、`POST mutate`（`{action: mkdir|rename|remove, path, to?}`）、`GET operations/:id`、`POST operations/:id/cancel`。路径均为别名内相对路径。PUT 必须有 `If-None-Match: *` 或强 `If-Match`，修改必须有 `X-Operation-Id`。重复 ID 返回 409；查询原 ID 获取结果，不重发内容。状态保留一小时，重启后未知。

每个请求独立完成，无 begin/end/commit 命令；分页以 `nextCursor: null` 结束。取消读取通过断开 fetch/响应流；修改通过取消端点和 deadline。提交后的取消不回滚。`committed` 表示原子命名空间替换可见，不承诺断电持久性或跨文件事务。

当前边界：上传最多 256 MiB；驱动完整读取默认 32 MiB；stat 每批 256 项；目录每页 512 项、单目录扫描上限 10 万项；全局最多 16 个文件工作槽。目录分页每页重新扫描，不是快照。非 UTF-8/特殊文件返回诊断；VFS 旧列表适配器遇到诊断会报不支持。禁止链接遍历和 `.itookit-upload-` 保留名称；独占启动时只恢复 SQLite 中登记的暂存文件，不递归扫描导出目录。路径必须是导出内相对路径：允许名称中的冒号（如 `2024:Q1.md`），拒绝盘符前缀（如 `C:/host`）。新建文件权限 0644、新建目录 0755；条件替换保留被替换文件的权限位。`remove` 当前仅支持文件和空目录；递归删除、append/patch、订阅和搜索端点未开放。外挂项目的 Agent 不装配本地 Shell/TTY，防止进程工具读到同名宿主目录。

### 上传暂存索引

“上传”指客户端保存远程文件、SeqFile 写入或同步内容落盘。正文暂存在目标文件同目录的 `.itookit-upload-<随机值>` 文件中，再通过原子重命名发布；SQLite 只保存文件位置和身份元数据，不保存正文。

每个可写导出持有独占目录锁，在保留目录 `.itookit-upload-state/` 中维护 `uploads.sqlite`。目录权限 0700、文件权限 0600，对远程文件接口隐藏并拒绝访问；只读导出和权限检查不创建索引。索引使用 SQLite 完整同步事务，创建暂存文件前记录意图，创建后登记父目录和文件的设备号、inode；发布或取消后先同步目录，再删除记录。暂存对象存活期间继续持有导出锁。

启动恢复按主键每批读取 128 条未完成记录，只检查对应目录和暂存名称。已消失的暂存文件只删除记录，已发布目标保留；身份匹配的暂存文件清除后再删除记录。目录无法访问、移动或被替换，文件被替换、变成链接，或崩溃发生在创建文件后、登记文件身份前时，保留文件及记录，通过 `uploads.recovered` 的 `retained` 数量告警，不阻止其他目录提供服务。数据库损坏、权限错误或版本不兼容会拒绝可写启动，并保留错误码。

旧版本未登记的 `.itookit-upload-*` 文件保留，不为迁移执行全目录扫描。索引属于服务内部状态，应随导出一起保留；可将 `.itookit-upload-state/` 加入宿主仓库的忽略规则。

```bash
cargo test                                  # 本项目根目录
pnpm --filter @itookit/nestos-node-driver test  # 在 itookit monorepo 内
```

驱动测试在 Linux 启动真实 Rust 服务，验证协议、条件保存和 VFS 适配；其他平台跳过该服务端集成测试。设计和验收边界见 [设计文档](../../doc/design/vfs-http-driver.md)。

包含远程来源的项目显示独立远程图标。服务断线时，仅关联项目的文件操作和新会话置灰禁用，已有会话仍可查看，其他项目继续可用；Settings 的重连入口保持可用。恢复连接后解除禁用。

## nestos-node 增量接口

`GET /v1/capabilities` 需要认证，返回安装身份及文件/同步/进程/终端支持情况。可在配置顶层指定稳定的 `server_id = "my-agent-node"`；未配置时为启用执行的服务生成本次启动的随机节点标识（不保证跨重启不变）。命令执行默认开启，配置顶层 `execution = false` 可切换为纯文件服务。启用执行时，启动会验证 Linux bubblewrap、fd 挂载及 user namespace 支持，失败即退出。`sync.push` 在同步服务开启且健康时为 true，详细能力通过 `/v1/sync/capabilities` 查询；`terminal.pty` 仍为 false；普通命令不依赖工作区租约模块。

`.gitignore` 由 NestOS 客户端文件树处理，服务端列表与文件访问不自动过滤。


### Remote commands

NestOS 项目右键菜单选择“启用远程命令”后，File Tools 和 Bash 共用远端 `/workspace`；本机 Shell 不作为回退。命令只挂载认证身份允许的 export 子目录，环境不继承服务端凭据。需要 `/usr/bin/bwrap` 支持 `--bind-fd`、`--ro-bind-fd`、`--disable-userns`。以非特权用户运行；尚不提供 cgroup 资源配额或多租户加固。

`POST /v1/processes` 启动（serverId、epoch、requestId、command、args、cwd、mounts、timeoutMs），`GET /v1/processes/:epoch/:id` 查询，`POST .../cancel` 取消。epoch 来自 capabilities.processEpoch。mount 使用 `{ alias, path, at, access }`，path 为 alias 内相对路径，绝不接受宿主路径。重复 requestId 不再次执行；重启后旧 epoch 被拒绝。取消返回 running 时仍需查询，直到进程确已回收。

每条命令最多一个 rw export（其他 export 为 ro）；该 writer 的独占锁保留给 monitor。单命令、最长 300 秒、stdout/stderr 各 64 KiB，超出输出上限会取消。返回最终有界输出，尚无实时流或 PTY。首版每次启动最多 1024 条进程记录，满后拒绝启动，需要管理员重启；重启不提供结果续接。

命令持有文件/进程互斥门，运行期间所有文件 API 返回 EBUSY；下载流、上传与后台提交保留其门直到结束。命令退出/取消并回收后使所有旧 revision 失效，避免 Bash 改动绕过条件写入。exclusive export 不允许其他宿主进程并发写入。执行锁在 monitor 中保留，daemon 意外退出后锁随进程清理释放。

验收：`NESTOS_NODE_PROCESS_TEST=1 cargo test`（要求可创建 Linux user/PID/network namespace）。

## 项目多端同步

同步存储、配置、管理员备份恢复与 HTTP 协议见 [单节点同步存储](doc/sync.md)。纯同步实例可使用 [config.sync.example.toml](config.sync.example.toml)，无需配置 export；sync.root 不存在或为空时首次启动自动初始化，也可先用 `nestos-node sync init CONFIG` 显式初始化。同步库与 export 使用独立目录，不自动发布工作目录的变化。

## 代码结构

依赖只向下：`http` 依赖领域模块，领域模块依赖 `core`；策略与机制分开放在不同文件里。

| 目录 | 职责 |
| --- | --- |
| `core/` | 认证、日志、错误、标识符、文件门闩、线程预算与分页机制 |
| `cli/`、`admin/`、`config/` | 部署配置、本地管理及旧格式适配 |
| `app/` | 共享宿主 State 与通用插件生命周期；业务状态通过 Services 注册 |
| `plugin/common/` | 插件与配置接口、依赖图、服务容器、协议聚合、租约机制及共享测试 |
| `plugin/catalog.rs` | 产品默认插件目录与兼容装配 |
| `plugin/files/` | 目录能力、文件操作账本、HTTP 文件接口和配置 |
| `plugin/process/` | 命令准入、沙箱、进程监管和 HTTP |
| `plugin/sync/` | 同步存储、维护、传输和同步工具契约 |
| `plugin/projects/` | 项目目录、授权、目录同步集成和项目工具 |
| `plugin/harness/` | 会话状态与工具；drivers/ 包含 Codex、Claude、DSH |
| `plugin/mcp/` | 通用 MCP 传输、发现与资源适配，功能由插件贡献 |
| `http/` | 共享认证、CORS、缓存策略、范围读取与宿主能力发现 |

策略与机制的分界举例：`fs::path` 决定「什么路径合法」，`fs::export` 决定「如何用 `openat2` 打开」；`process::policy` 决定「哪些挂载被授权」，`process::sandbox` 决定「如何拼装 bwrap 参数」；`workspace::lease` 管租约状态机，`workspace::journal` 管 `write → fsync → rename → fsync` 的持久化与故障分级；`operations` 管收据语义，handler 只负责解析与转发。

进程退出分两级：先停止接收命令、请在工作中的操作停止并等待文件门排空（5 秒），再给已打开的连接 5 秒自行结束，避免慢速下载无限拖住 Ctrl-C。

测试按功能放在 `src/plugin/<id>/tests/`，通用框架和共享脚手架位于 `src/plugin/common/tests/`。Cargo test target 名称保留，支持 `cargo test --test contributions`、`cargo test --test sync` 等独立验证。

## 日志

启动首先在终端打印 `nestos-node <版本号>`，配置或初始化失败时也能看到运行版本。`--version` / `-V` 只打印版本并退出，不读取配置或启动服务。日志初始化后先记录 `server.starting`（version、config、logFile），就绪时 `server.ready` 继续记录相同版本，启动失败事件也带版本。正常启动事件遵循 `log_level`（info 及以上详细级别可见）。

顶层 `log_level = "info"` 为默认值，可设 `trace/debug/info/warn/error/off`。服务事件同时输出到 stderr 和配置文件所在目录下的 `logs/nestos-node-<启动时间>-<PID>.jsonl`；启动终端打印绝对日志路径，`server.ready` 也记录版本和路径。以 `tools/config.toml` 启动时，默认目录为 `tools/logs/`。启动提示和 API Key 等终端文本不复制到文件，文件只接收结构化事件。

默认单文件 10 MiB，按完整记录轮转为 `.jsonl.1` 至 `.jsonl.4`，每次启动最多保留 5 份（包含当前文件）；旧启动的日志保留供排查，不自动删除。单条记录不截断，可能独自超过大小阈值。日志文件权限为 `0600`，新建目录为 `0700`。事件直接写入文件，不逐条 `fsync`；日志初始化失败时启动报错，运行中写入失败则提示终端并继续 stderr 日志，不中断任务。

可选配置（相对目录以配置文件所在目录为基准，绝对目录也支持）：

```toml
[logging]
directory = "logs"
max_file_bytes = 10485760
retained_files = 5
```

事件使用 JSON 行；每行首先输出带毫秒和本机时区偏移的可读 `time`，再输出 level、event、fields，末尾保留 Unix 毫秒 `timeMs` 供程序处理。例如 `{"time":"2026-10-08T13:44:42.034+08:00","level":"info","event":"server.ready","fields":{"address":"127.0.0.1:8787"},"timeMs":1791438282034}`。时区使用服务进程的本地设置，可通过 `TZ` 调整。sync.diagnostics 是每分钟的调试性能汇总，仅在 debug/trace 下输出；普通运行保持 info 即可。

- debug：文件变更/命令准入；info：启动就绪、变更提交、命令启动与成功结束、取消和关闭。
- warn/error：认证拒绝、HTTP 错误、命令非零退出、超时、启动或清理失败；保留操作/请求 ID、状态和退出码。
- 原生 Codex RPC 失败输出 `harness.native.failed`：method、rpcCode、稳定 code、durationMs，以及 sessionId、profileId、projectId、ownerId、processId、childPid、nativeRpcId；来自修改操作时携带原 requestId/operationId 和 epoch。字段缺失保持 null，不记录原生错误全文、输入、路径或凭据。`harness.operation.started/completed` 记录修改执行及持久回执的 committed/not-committed/unknown；`harness.native.completed` 记录原生线程创建、恢复和归档等成功。`harness.native.started/stopping/stopped` 记录子进程生命周期、停止原因、回收结果及退出码；子进程终止不证明外部写入者释放锁。`HARNESS_SESSION_BUSY` 表示其他原生实例持有会话写入锁；`HARNESS_INVALID_PARAMS` 对应 -32602，其他未识别原生错误保留 `HARNESS_RPC_ERROR`。读取历史成功不代表能恢复写入。
- Codex 后台可能在客户端离开后继续保留会话；写锁不证明某个前台客户端仍在使用，持久化 source 字段也不能识别当前锁持有者。锁拒绝后保留输入，不删除锁文件或强制并发写入。
- `harness_session_info` 与会话历史返回可选 `session.writerLock`，这是独立于历史占用错误的只读 Linux 锁检查：`status=held/not-held/unknown`、`checkedAt`、持有者 `owner.pid/uid/processStartedAt`，可选 `lockFileModifiedAt` 和安全错误码。锁文件设备号与 inode 对照 `/proc/locks` 的实际 FLOCK WRITE，复查持有者变化；文件存在不代表被锁，准确取得锁的时间无法获知，`acquiredAt=null`，不得拿文件修改或进程启动时间代替。旧观察不证明当前占用，失败指令仍须用户重新提交。`harness.writer_lock.observed` 保留操作身份、会话、阶段、结果、PID/UID、错误码及耗时，不输出物理路径、命令行或正文；权限不足、锁表超限和检查变化返回 unknown，不阻断普通会话读取。
- 原生执行记录 `harness.turn.started/completed/failed`，流错误记录 `harness.stream.error`。保留 sessionId、turnId、状态、重试标记及原生提供的错误类别/HTTP 状态码；不复制消息、工具内容或原生错误正文。可与 `http.failed` 的路径、状态和时间对照，区分任务失败与文件接口繁忙。
- 普通读取、stat、目录列表、能力查询及状态轮询成功时始终静默，包括 trace/debug。读取错误输出 `http.failed`。
- 不输出 Authorization、口令、请求正文、命令正文或文件内容；命令输出仅记录字节数。进程结束记录是回收后的结果，HTTP 断线不冒充操作已取消。


## SQLite SeqFile

在 export 内通过 `POST /v1/fs/:alias/seq/snapshot` 读取 `{path}` 指定的 `.seq`，返回 `{revision,entries:[{key,value}]}`。`POST /v1/fs/:alias/seq/transaction` 使用 `X-Operation-Id`，请求为 `{path,expectedRevision,changes}`：变更项为 `{action:"set",key,value}` 或 `{action:"delete",key}`。创建时 revision 为 null；更新时必须携带读取的 revision。

每个 SeqFile 是可复制的 SQLite 数据库。服务端执行结构化单文件事务，再复用条件文件替换与持久回执；不接受任意 SQL。读取缺失文件不创建文件，父目录需提前创建。只读 export 禁止写入。每文件最多 16 MiB、每批最多 256 项、key 最多 1024 字节，HTTP 请求体仍有独立上限。并发冲突返回 ECONFLICT；unknown 结果使用现有操作查询确认，不自动重放。此接口与 sync 对象库无关，不提供跨 SeqFile 事务。

## 原生 harness 控制中心（MCP 2.0）

### Harness 插件扩展

`harness_operation` 按原 epoch/requestId 查询回执。当前 epoch 内尚未准入的请求返回 `not-committed` / `HARNESS_REQUEST_NOT_STARTED`，并原子保留拒绝记录，使迟到的同 ID 修改无法执行。已准入请求沿用原回执，执行中仍为 unknown。

回执账本与原生运行时独立：项目回执按稳定 owner、projectId 和规范 profileId 隔离，不按目录 revision 或 readOnly 分库。重新挂载、配置失败及空闲回收不会删除回执；查询核对当前主体与项目授权，允许查询原 revision 的操作，但不允许借此执行过期授权下的新请求。同一 requestId 更换正文仍拒绝。

账本使用当前服务 epoch 内的私有 SQLite 临时库，数据库缓存预算 2 MiB、主库上限 256 MiB；不再按已完成操作的累计条数拒写。1024 项限制改为整个 harness 服务正在执行的修改操作数，执行收尾后归还名额；回执查询不占执行名额。数据库满或存储故障不清除防重身份；返回 `HARNESS_RECEIPT_STORAGE` / unknown，已准入操作保留未知记录，不能重发。临时库不提供跨服务重启恢复：新进程使用新 epoch，旧 epoch 继续拒绝，不能从缺失记录推断旧执行结果。

服务使用编译时注册的 `HarnessPlugin` / `HarnessPlugins` 插件接口。默认注册 Codex、Claude Code 和 DSH；`kind` 由注册表识别，未注册插件在启动验证时拒绝。插件负责原生协议、会话存储读取、进程初始化及统一会话/事件/交互格式，公共服务负责 epoch/requestId、回执、串行修改、项目授权、实例缓存和关闭。项目 launcher 接收插件指定的启动参数，不再固定 Codex 参数。

新增实现可通过 `HarnessPlugins::register` 注册，使用 `State::from_config_with_plugins` 装配；标准二进制的内置插件在注册表默认实现中声明。普通实例与项目实例都通过同一个插件工厂创建，项目实例接收 `ProjectRuntime` 并使用共同目录授权及 launcher。插件是可信的进程内代码，当前不支持动态库加载或外部插件自动发现。Claude Code 使用 SDK 协议；DSH 使用与 dsht 相同的 Remote HTTP/WebSocket 协议，配置未知 kind 仍会拒绝启动。

### DeepSeek Harness（DSH）

`kind = "dsh"` 适配 DSH 0.2.0-rc.2 的 `web` profile，协议依据 `deepseek-harness/tui/src/transport/` 及宿主 Session Controller。示例见 [config.dsh.example.toml](config.dsh.example.toml)。无需安装 dsht；Rust 驱动直接交换登录 cookie、调用 HTTP Remote 方法并读取 WebSocket mux，不解析 TUI 屏幕。

已有多身份服务可将 [profiles/dsh.example.toml](profiles/dsh.example.toml) 追加到现有配置，替换实际 command/home，并绑定已有 `principal_id`。`home` 应指向原来的 DSH_HOME；新建空目录不会带入历史会话。0.3.3 的项目列表通过 ProjectRuntime 校验原生 workspace 的物理路径，兼容既有物理目录与 `/projects/<id>` 虚拟路径，并继续拒绝项目之外的会话。配置及二进制更新后重启 nestos-node，再在客户端刷新 MCP 能力；不需要复制历史或重建项目。

默认 profile ID 使用 `codex`、`dsh` 等类型名；Codex 追加式模板见 [profiles/codex.example.toml](profiles/codex.example.toml)。改名可配置 `aliases = ["codex-main"]` 保留旧引用，发现列表仅返回新 ID 及 aliases 元数据。服务先将别名解析为规范 ID，再校验主体权限、选择项目实例及处理回执；旧名称和新名称共享同一实例，不创建重复会话或额外控制进程。别名必须是有效且全局唯一的 profile 标识，不能覆盖其他 profile 或别名。

0.3.4 的 DSH 项目启动保留授权目录的原物理路径作为兼容别名。只有虚拟路径时，DSH 原生 workspace 会将无法规范化原 cwd 的历史成员过滤为空，即使 session/list 已读到原日志。通用 `ProjectRuntime.prepare_with_source_paths` 从已固定身份的挂载生成等权别名，保留根和嵌套挂载的读写限制，不开放父目录或其他 export；别名再次经过通用目标与权限校验，不允许遮蔽 /tmp 等运行时目录。普通进程与其他驱动的默认命名空间不变，DSH 不绕过成员关系检查。

0.4.2 为 DSH 历史中的完整 turn 增加派生的终态与可选 submissionEvidence（version=1、requestId、promptSha256、起止 seq）。证据只来自同一有界历史页的连续事件、完整 turn/start/end 和唯一原生用户输入；分页截断、事件缺口、替换或多输入不提供单次提交证明。不持久化另一份状态、不启动或接管原生会话；客户端还必须与原已接纳队列的身份和输入摘要匹配，才能结算旧 turn。

0.4.1 修复 DSH 真实事件的 turn 关联：`user/message` 只携带消息身份、内容和来源，不要求额外 `turn` 字段；适配器在同一连续事件流中用已核对 requestId/内容的用户消息关联前面的 `turn/start`，再按对应 `turn/end` 确认完成。已关闭的 turn 不用于后续提交，正文相似、原生 idle 或助手输出均不能代替完成确认。测试替身使用相同原生消息形状；否则会出现回复已完成但活动 turn、项目执行租约和命令队列一直未释放。运行中的 node 必须更新至此修复版本，更新 httpd 本身不会改变原生适配器。

0.3.5 的历史投影使用持久事件中的原生 turn 编号，不再将不同轮次合并；分页从轮次中间开始也保持相同身份。实时流仍沿用当前提交的 requestId。DSH 会话额外返回可选 archiveAllowed，允许客户端显示空闲但未附着会话的归档操作；不以 owned 推测原生管理权限，归档提交仍再次核验原生运行状态与目录授权。

默认托管模式显式配置 `command` 和已存在的私有 `home`（DSH_HOME），由 nestos-node 延迟启动 `dsh --patch <临时传输配置> --profile web --no-open --host 127.0.0.1 --port 0`，读取 readiness URL 后在内存交换 cookie。临时 overlay 在私有 home 内使用 0600 权限，固定回环地址、随机端口、readiness 输出及禁止打开浏览器，避免用户 profile 的固定绑定覆盖启动参数；进程回收后删除 overlay。请先在该 home 完成 DSH profile 初始化、模型与凭据配置；启动超时默认 120 秒。项目模式 `projects = true` 使用共同 ProjectRuntime/launcher、目录 grants 与文件活动租约；`[projects].network = true` 允许本机控制连接和模型网络。DSH 应安装在沙箱可访问的系统目录；源码开发启动器可能引用沙箱外路径，不能直接当作可部署命令。

已有服务模式设置 `[harnesses.dsh].url`（仅 HTTP(S) origin）及 `token_env`，令牌只在服务端环境解析。该模式必须使用显式 legacy workspaces、`projects = false`，且不能用于多身份项目运行时；已有 DSH 的工具权限由它自己的配置决定。不会结束已有服务进程。DSH 服务需允许该 URL 的 Host；HTTP(S) 与 WS(S) 均使用同一 origin 和 cookie。

支持会话列表、历史分页、新建、继续、流式文本/推理、工具摘要、审批/问题回复、中断、重命名及归档。所有变更复用通用 epoch/requestId 回执；原生新建也携带确定性的会话 ID。HTTP 只等提交回执，模型执行由后台流推进；不同会话不共享模型等待锁。默认最多 64 个附着会话、128 个在途 HTTP 请求；WS 队列和重放缓冲有界。网络超时或无法确认的原生错误保持 unknown，不自动重发 prompt。

普通输入的回执为 unknown 时，`harness_operation` 可向仍存活的原驱动读取接纳证据：原生 `user/message` 必须有匹配的 `source.kind=user`、`source.rpcId`、turn 编号与冻结文字摘要。身份或输入不一致不会确认，输入冲突返回 `DSH_PROMPT_EVIDENCE_MISMATCH`。证据先写回同一 epoch 回执库，再解除接纳占用；迟到超时/拒绝不能覆盖已确认回执或清理正在运行的新旧轮次。查询不会初始化新驱动、resume 或重新发送输入；项目模式按相同主体/项目/profile 回执作用域查找既有实例，兼容观察模式变化。

拥有明确原生消息身份后才绑定本次 turn；外部 turn 的内容使用其原生身份，不提前完成本次调用。尚未识别输入时取消返回 `DSH_TURN_NOT_IDENTIFIED`，不会发送宽泛的会话级 cancel。取消回执未核对时保留独立控制占用，后续写入返回 `DSH_CONTROL_RESULT_PENDING`；成功取消提交与真实终态分开，原生正常完成不会被请求意图改写为 cancelled。原生服务的外部写入和条件取消仍受其能力限制，本地校验不提供远端 CAS。

容量不足时，仅回收已经观察到终态、没有借用中的控制操作、待审批、活动 turn 或未核对命令的观察订阅。先等待订阅任务退出，确认后才释放槽位；无可回收对象返回 `DSH_SESSION_LIMIT`，退出未确认返回 `DSH_SESSION_CLEANUP_PENDING` 并继续计入容量。新建后尚未执行的会话保留槽位，避免创建与首次发送之间被回收。回收不调用原生 cancel/archive，不删除历史、操作回执或上下文身份，也不代表 DSH 原生会话或工作区已被销毁。

回收后目录返回 `owned=false`，新一轮需要恢复同一个 sessionId。客户端的队列派发准备会先 inspect，再按需 resume；读取历史不会自动恢复所有权。命令结果未知时返回 `DSH_COMMAND_RESULT_PENDING` 拒绝新的写入，状态查询仍可用；不能因界面已显示 idle 就清掉未知操作。容量日志包含操作身份、阶段、耗时、活动轮次/未确认命令/待交互数量，不记录输入或原生错误正文。DSH 的实时 requestId 与原生历史 turn 编号的完整持久映射仍需补齐，本次回收不提供跨事件缺口的完整恢复保证。

暂不声明 fork、永久删除、取消归档、附件或用量计价能力；这些操作由能力描述隐藏。断线后该驱动实例拒绝继续写入，重启 nestos-node 后可显式继续原生持久会话；不会自动重放未知操作。托管实例退出时回收其进程组，活动结果不明时按共同文件门闩机制拒绝继续访问。日志复用 nestos-node JSONL，记录生命周期与安全的 DSH 错误代码，不输出 URL 令牌、cookie、prompt 或原生错误正文。

命令目录与模型目录的读取失败不代表原生执行结果未知。Harness 服务在调用执行端口前保留明确的未派发依据；DSH 的新 `HarnessDriver.execute_checked` 还区分适配器内部预检与实际修改请求。能够证明未派发时，沿原回执保存 `outcome=not-committed`、原安全错误码、`stage=prepare` 和 `executionStarted=false`；队列保留失败条目，显式重试才分配新 attemptId。旧插件的默认实现保守视为已派发，不根据错误码推测未发送。模型目录读取在取得 prompt 占用之前完成；`selectModel` 或 `commands/execute` 一旦可能已发出，EIO/超时仍保持 unknown 和原占用，不能以“没有 prompt”推断修改未发生。回执持久化失败仍返回 unknown，不能用未派发内存事实绕过原回执核对。

DSH 串行命令使用独立的 `harnesses.dsh.command_timeout_ms`，默认 600000 ms，可配置范围 100–1800000 ms；普通查询继续使用 `request_timeout_ms`，默认 25000 ms。外层服务在 25 秒内未完成时仍可返回 unknown，但不会取消已派发的原命令工作者。原生 HTTP 响应须匹配原 rpcId；迟到的正常响应继续保存到原 epoch/requestId 回执，不重发命令。原生等待本身超时、连接断开或响应身份不符仍保持 unknown。

命令占用携带原 operationId，并保留已取得的完整规范化响应直到共享回执持久确认。回执写入失败时，`harness_operation` 可从同一存活驱动读取原响应，写回既有回执库，再解除占用及项目运行所有权。unknown、错误 requestId、不同原生结果以及迟到的旧回执均不能解除当前占用；成功响应不能被 not-committed 回执覆盖。独立状态查询不等待长命令；后续修改继续被 `DSH_COMMAND_RESULT_PENDING` 阻止。

服务的既有修改串行锁覆盖原生派发、回执持久化及驱动收尾确认；排在仍执行的原命令后的修改先等待该锁，不能在原结果保存之前取得执行权。原工作者已退出但其回执仍未知时，DSH 原操作占用继续拒绝冲突修改；查询和精确回应／取消仍走各自原有独立通道。

DSH 0.2.0-rc.2 的 `command/run.source` 只有 `kind=user`，原生 `commandId` 由其执行器生成，未携带调用方 requestId；真正丢失原生执行回复后，仍不能按名称、参数或相近时间猜测关联。上述原 HTTP 响应保留不提供跨原生断线、节点 epoch 或历史事件缺口的完整身份恢复，观察订阅回收也不解决该限制。

### 原生命令的大输出

`harness_profiles` 的 `capabilities.commandOutputs=true` 由公共服务声明，不要求每个原生驱动实现产物读取。客户端在每次 `harness_command` 或 `harness_operation` 请求显式传 `outputMode="reference"` 才启用引用投影：命令输出超过 64 KiB UTF-8 时，公开回执返回不超过该上限、按字符边界截取的 `result.output` 预览，并附 `result.outputRef={version:1,epoch,requestId,bytes,sha256,mimeType:"text/plain"}`。失败标志、commandId 和实际终态保持原值，小输出沿用原格式。

省略 outputMode 保持完整原输出，兼容不识别 outputRef 的旧客户端；其他 outputMode 值明确拒绝。该字段仅影响对外展示，不发送给原生执行，也不计入 harness_command 的执行 fingerprint。同一 requestId 可在完整与引用模式之间核对或重试，不重放命令；其他修改操作的参数与摘要规则不变。两种模式都移除内部摘要元数据，旧客户端不会看到私有 `_commandOutput`。

完整原文继续存入同一 ReceiptScope 的原 `result.output`；既有事务同时保存私有字节数和摘要元数据，公开回执不暴露该内部字段。驱动的 `operation_settled` 收到完整原结果，不能用预览代替，否则 DSH 的原生结果相等核对会失效。原回执不可被迟到工作者覆盖，产物读取失败也不修改执行回执。

只读 `harness_command_output` 接收 `profileId/requestId/epoch/sha256/bytes/offset/limit`，项目作用域仍携带 `projectId/revision` 及可选 `readOnly`。`limit` 范围 1–65536 字节，返回 `{ref,offset,data,nextOffset}`，其中 data 是规范 base64，nextOffset 为下一字节位置或 null。字节块可能从 UTF-8 字符中间开始；客户端必须合并字节或使用流式解码，完整读取后核对总字节数和 SHA-256。offset 等于总长度时返回空块及 null。

读取严格核对当前授权作用域、项目修订、epoch、原回执 ID、摘要、字节数及范围，不接受本地路径或临时 URL。它直接查找现有回执，不为缺失 ID 创建 not-committed fence，也不创建原生驱动、恢复会话、取得 writer 或授予控制权。原项目驱动已关闭时仍可在该 epoch 读取已保存结果；其他项目或 profile 不能复用同一个 requestId 获取正文。

保留期仍是原节点 epoch 的私有临时 SQLite；重启后旧引用明确失效，不冒充永久历史或已导入宿主 Artifact。新记录在提交时计算一次摘要，分页仍会读取、解析原 receipt JSON；旧记录缺少摘要元数据时按实际正文计算。当前 DSH 原生传输的 8 MiB 响应上限仍有效，这不是无限大小或跨重启输出存储。

0.3.1 的真实连接验收使用已发布 piagent-driver@0.2.0 → MCP → Rust DSH 驱动 → DSH 0.2.0-rc.2 → DeepSeek `deepseek-v4-flash`。验证新建、跨轮记忆、4 会话同时执行和流式增量隔离；SIGTERM 后托管 DSH 被回收，重启后继续原有会话仍保留上下文。共 10 个模型 turn 成功并持久记录 usage，测试会话全部归档。永久删除实际返回 ECAPABILITY 且保留会话，不计作删除成功。验收发现并修复用户绑定覆盖 CLI、新建 DTO 缺少 nullable updatedAt、SIGTERM 遗留原生写锁三个问题。

节点处理 SIGINT 和 SIGTERM，先 fence 并回收所有托管 harness，再关闭文件与连接；DSH、Codex 和 Claude 共用这一关闭入口。已有外部 DSH 进程不归节点管理，不随节点停止。

启动时（日志级别 info 或更详细）输出可复制的 MCP endpoint、serverId、认证方式、凭据来源及实际 API Key；旧 Basic 配置只显示用户名和密码来源。监听通配地址时另外尝试显示默认路由的本机 IP 候选；多网卡、容器、NAT 或代理场景仍需填写客户端实际可达地址。该探测不发送 UDP 数据包，也不发现公网地址。

认证使用每请求的 HTTP Basic 或 Bearer，没有 cookie 登录会话和自动到期时间；配置凭据不变时，服务重启后仍有效。itookit 的 nestos-node 项目绑定支持标准 MCP API Key（Bearer）和旧 Basic 配置；API Key 使用 MCP 既有存储，驱动运行时只保留内存副本。API Key 不写入项目挂载记录，MCP 导出仍移除 API Key。

使用 [config.harness.example.toml](config.harness.example.toml) 配置。`harnesses` 缺省为空，不启动或访问 Codex；每个 profile 显式设置已存在的绝对 CODEX_HOME 和授权 workspace。运行 nestos-node 的系统用户需已有 Codex 登录状态及已安装的 CLI。要浏览当前用户 ~/.codex 的会话，填写该目录的实际绝对路径；TOML 不展开 `~`。不读取未配置的其他 home，不把 auth.json 等密钥文件作为会话返回。

`execution = false` 关闭原有 shell 服务，但仍可启用 harness；无需 exports 即可仅运行控制中心。workspace 必须与 exclusive rw exports 分离（启动时拒绝重叠），可用 ro export 浏览相同目录。`server_id` 可设置稳定服务身份。

Web/Tauri 在工具箱 MCP 配置并测试连接后，在支持 harness 的 nestos-node 配置中点击“控制中心”。支持 profile/workspace、新建、分页/归档列表、历史、继续、发送、增量输出、命令/文件审批、用户问题、显式中断和未知请求收据确认。历史浏览不会接管会话；继续仅允许授权 cwd、拒绝接管其他宿主的活跃线程。关闭面板只释放客户端连接，服务端 turn 继续运行。CLI 宿主已统一 adapter 接入，独立 harness 命令行界面尚未提供。

服务提供认证后的 `POST /mcp`：实现 MCP 协议 2026-07-28，与 MCP 客户端 SDK 2.0.0 对接，支持 server/discover、tools/list/call、resources/list/read、resources/templates/list、ping；不宣告 Tasks 扩展。发现工具为 nestos_node_capabilities（兼容 piagent_capabilities / fsagent_capabilities），其余工具按项目、目录同步和 harness 分组。请求验证 protocol/version、method/name 和客户端 metadata；clientInfo 可省略，携带时验证 name/version。带 Origin 的请求还验证允许列表。Codex 控制采用官方 app-server stdio JSON-RPC，命令和目录只来自服务端配置。

`tools/list` 根据部署配置与当前身份返回可用工具：项目服务未配置时不列出项目工具，目录同步只对所属身份开放，harness 只对有 profile 授权的身份开放；project_exec/project_search 还检查执行服务与搜索程序是否可用。相同工具的参数契约固定，不因连接、会话或调用顺序变化。每个工具仅声明实际支持的必需/可选字段；project_search 的 mode 为 name/path/content，harness_session_search 为 title/content/prompt。项目范围的 harness 参数用 dependentRequired 声明 projectId/revision 的关联，具体 profile 的原生能力继续由 harness_profiles 返回。未知工具和畸形 tools/call 使用 JSON-RPC 错误；业务输入与执行失败仍返回 isError。

### MCP 目录资源

`server/discover` 声明 `resources: {}`，不声明订阅或列表变化通知。`resources/list` 返回一个 `application/json` 资源 `nestos-node://<serverId>/directories`，名称为 accessible_directories；通过 `resources/read` 按需读取，不会自动扫描或附带文件内容。未显式配置 server_id 时沿用认证层生成的安装身份。首版没有参数化资源，resources/templates/list 返回空数组。列表和读取结果均为 ttlMs=0、cacheScope=private；HTTP 响应继续使用 Cache-Control: no-store。

读取结果为 version=1 的 JSON 文本，包含 serverId、authorization（principalId/revision）和以下集合：

| 字段 | 内容与路径语义 |
| --- | --- |
| exports[] | 当前身份可读的 alias、path=""、逻辑 virtualPath（例如 /docs）、有效 access、nameSemantics、strongRevision、authorization 与 capabilities.fileRead/fileWrite/run |
| projects[] | 当前身份拥有且通过现时授权/目录身份校验的 id、name、revision、root、mounts[] 和项目 capabilities |
| root / mounts[] | 来源 alias、export 相对 path、逻辑 virtualPath、执行空间位置 at（根为 /workspace）、有效 access 与文件/执行 capabilities；附加挂载保留各自权限 |
| workspaces[] | 当前身份获授权的旧版 harness profileId、workspaceId 和 capabilities.sessionCreate；workspace 标识本身不授予 HTTP 文件访问权限 |

例如项目 root 可以是 `{ "alias": "docs", "path": "notes", "virtualPath": "/docs/notes", "at": "/workspace", "access": "ro", "capabilities": { "fileRead": true, "fileWrite": false, "run": false } }`。逻辑 virtualPath 不是宿主绝对路径，at 只用于执行空间；run 同时检查服务就绪与身份执行授权，项目还要求所有挂载均获执行授权。权限描述仅用于发现，后续文件、项目与 harness 操作仍逐请求鉴权并校验自己的修订号。

资源读取复用文件 worker 的并发预算、超时和共享 gate，按本次请求的身份重新投影目录；授权已撤销、符号链接或物理身份已替换的项目不进入清单。不返回宿主物理路径、harness home、内部目录身份、数据库目录、凭据或文件内容。未知资源 URI 返回 -32602；读取的 Mcp-Name 必须与 params.uri 一致，支持标准 Base64 header 编码。跨身份读取同一个 URI 会得到各自的授权清单。

MCP 代码分为 [传输入口](src/plugin/mcp/mod.rs)、[协议校验](src/plugin/mcp/protocol.rs)、[方法路由](src/plugin/mcp/requests.rs)、[通用工具分派](src/plugin/mcp/tools/mod.rs) 和 [资源适配](src/plugin/mcp/resources.rs)。工具 Schema 归各插件的 tools.json，调用、权限、能力和错误语义由各自 contribution.rs 提供；目录投影同样按插件聚合。相关协议和权限回归见 [MCP 测试](src/plugin/mcp/tests/mcp.rs)，独立插件扩展见 [contributions 测试](src/plugin/common/tests/contributions.rs)。

已验证 Codex CLI 0.159.2。项目沙箱在配置的真实绝对路径挂载 harness home，并保留 `/harness` 兼容入口，避免原生 SQLite 索引中的绝对日志路径失效。`harness_session_info` 只读取元信息；普通会话历史优先原生 API，paginated 会话及 `toolDetail: "summary"` 请求读取 home 下 sessions/archived_sessions JSONL。历史使用 8 MiB 的日志窗口，必要时通过有界缓冲向前查找真实 turn_context/task_started，返回最多 100 项/2 MiB，通过 `nextCursor` 加载更早内容。保留真实 turnId、时间和消息内容数组，兼容 custom_tool_call；摘要模式在分页预算前去除工具参数和输出，只返回工具名称、操作、首个非空命令行（最多 240 个字符）和目标文件。事件也支持摘要模式并保留原始游标及审批请求。原生 createdAt/updatedAt 分别统一转换为毫秒，缺失时间返回 null；空名称回退原生请求预览，branchName 独立返回；session.native 不重复附带整份 turns。路径禁止符号链接，未完成尾行等待刷新；不修改原生索引或日志。继续会话使用 `thread/resume` 的 `excludeTurns`，由原生 CLI 确认可用性。

`harness_fork` 调用原生 `thread/fork`，可选名称经 `thread/name/set` 保存，返回 `parentSessionId`。仅授权项目的可写会话允许创建分支；空会话、活动会话和未知结果期间 UI 禁用创建。分支创建同样携带 epoch/requestId，并以回执避免重放。分支是独立的原生会话，历史保留在 harness home。

限制：harness 服务最多 1024 个同时执行的修改操作，回执临时主库最多 256 MiB；每原生运行时 128 个 pending 调用/交互、事件环最多 1024 项/8 MiB、待审批总量最多 8 MiB、每轮提示最多 128 KiB、原生单行最多 8 MiB；客户端 MCP 响应最多 32 MiB。输出 gap 显示需要刷新历史。发送结果未知不重放；epoch 改变表示服务已重启，旧收据不再可确认。app-server 退出后本 profile 拒绝继续写入；需服务重启重新发现，避免悄悄失去接管关系。服务关闭回收监管进程组；主动脱离该组的后代不属于此回收保证。

项目模式允许同一项目或不同项目的多个原生会话同时执行，每个项目 profile 最多 128 个活动 turn。同一会话仍只允许一个活动 turn，提交队列按会话串行；一个会话等待启动审批不阻塞其他会话。原生子树归档和删除与所有在途提交互斥，审批回复和中断继续旁路，以免等待中的启动无法完成。

活动 turn 各自持有文件保护租约，共享全局排他的原生执行活动组；最后一个 turn 结束或对应进程确认回收后才开放文件 API、project_exec 和项目挂载变更。完成事件必须匹配 session 与 turn；在启动回执之前到达的完成事件有界保留，绑定实际 turnId 后才释放，迟到事件不能结束下一轮。某个 Claude 进程退出只回收自身会话，Codex app-server 回收该实例的全部会话；未确认进程回收时继续封闭文件门闩。并发会话共享项目工作区，独立代码修改应显式使用 worktree；服务不自动创建或选择 worktree。

Codex app-server 的 stdin 写入和 stdout 读取独立推进；大 prompt 的管道背压不会停止接收其他会话的增量事件和 RPC 回执。请求仍使用有界队列与最多 128 个在途原生调用。会话写入按 session 串行；删除/归档的拓扑锁发生冲突时返回 EBUSY，不排队占住共享拓扑锁，避免一项待授权操作和随后排队的删除拖住其他会话。Claude 初始化仅持有当前会话槽位的锁，最多 16 个槽位同时覆盖已启动和初始化中的会话；未完成握手的会话不阻塞其他会话的控制或实时列表。项目关闭先 fence 对应驱动，再释放共享表锁等待回收；用量文件读取只锁当前会话的游标。

Claude 槽位已满时，先回收最久未活动且没有活动轮次、待处理原生交互或调用借用者的会话进程。进程退出确认后才允许复用容量；关闭等待超时返回 `CLAUDE_SESSION_CLEANUP_PENDING` 并继续占用槽位，全部会话仍活动时返回 `CLAUDE_SESSION_LIMIT`。回收保留原生历史与操作回执；冷会话需显式 `harness_resume` 后再提交新轮次，不将普通读取变为执行。

原生输出通过 `POST /v1/events` 的 fetch SSE 推送。一个应用运行时按节点与身份共享连接，Harness、目录和进程通过 `{id, plugin, args}` 复用；修改命令和回执仍走 HTTP/MCP。Harness 沿用有界 epoch/cursor/gap 回放，文本事件满页立即追赶，断线按已消费游标恢复，缺口重新读权威快照。宿主会话与侧栏由事件触发更新，并保留 15 秒权威状态核对；进程终态通过通知等待。0.4 删除原事件及目录轮询工具，不提供旧节点回退，节点和客户端需配套升级。首字等待和工具停顿仍需结合 turn 生命周期诊断。

验证（在 itookit 根目录）：

```bash
cargo test --offline --manifest-path tools/nestos-node/Cargo.toml
NESTOS_NODE_CODEX_TEST=1 cargo test --offline --manifest-path tools/nestos-node/Cargo.toml --test harness
NESTOS_NODE_HARNESS_TEST=1 pnpm --filter @itookit/nestos-node-driver test tests/network.test.ts
```

第一项包含原生协议模拟测试；第二项用独立临时 CODEX_HOME 启动真实 Codex，只验证创建/历史，不调用模型，也不读取个人会话；第三项用真实 MCP SDK、Rust HTTP 服务和模拟 Codex 验证完整操作。

server/discover 的 `_meta['itookit/nestos-node']`（同时保留旧 `itookit/pi-agent`）返回文件协议版本、同源相对 HTTP 端点、安装 serverId、项目和 harness 支持；nestos_node_capabilities 返回同一描述并保留旧客户端兼容。客户端优先复用标准发现响应，不根据名字判断服务；该描述是本服务扩展，不是 MCP 标准保证。建议显式配置全局唯一且稳定的 server_id，以识别同一安装的不同访问地址。多个原生 harness 由服务端插件注册表适配，共用这一 MCP 连接；当前内置 Codex、Claude Code 与 DSH。


## 目录项目模式

[config.projects.example.toml](config.projects.example.toml) 开启服务端项目 catalog。导出根是项目选择范围，`project_register` 可以绑定其任意层级子目录，或在已有父目录下创建一个新目录。文件、bash 和项目 Codex 使用同一份目录及挂载授权，sync 数据项目独立。稳定 server_id 必须配置，catalog 和 native home 不可放入导出根。

MCP 提供 project_roots/list/read/register/configure/exec；harness 工具增加 projectId/revision/readOnly 上下文。新 harness profile 设置 projects=true，在外层 bubblewrap 内运行；命令应指向可独立运行的 native binary。projects.network 默认为 false；联网需显式开启。目标挂载目录先创建，再提交策略。无法启用沙箱时拒绝执行。

当前 FileGate 是安装级，turn 执行期间文件请求可能返回 EBUSY。VMM 和项目级并发尚未实现，已留共同 ProjectLauncher 端口。详细授权、兼容行为与验证范围见 [项目模型](../../doc/design/nestos-node-project-model.md)。

同时启用 `[projects]` 与 `[sync]` 后，目录绑定支持数据集落地、目录回传和双向同步。
使用 `project_sync_directories` 浏览项目内的已有子目录；绑定和预览不写入文件。
`project_sync_configure` 根据 policyRevision 改方向并使旧预览失效。
`project_sync_preview` 返回新增／覆盖操作、方向和包含两侧摘要的 conflictDetails。
`project_sync_compare` 对当前计划提供最多 256 KiB 的 UTF-8 文本及基线；二进制和大文件只返回说明。
`project_sync_resolve` 选择 dataset／directory 来源并生成新的 planId，复核后使用 execute 确认。
回传复用 sync 的 CAS 发布与持久回执，关闭面板或未知结果时通过 status 继续同一计划。
默认更新模式保留目标独有文件，不传播删除、不穿透附加挂载或 .nestos；镜像删除未开放。

服务由 pi-agent 更名为 nestos-node，更早名称为 fs-agent。升级时继续使用原 config.toml、server_id、API Key、projects.root、sync.root 和 harness home；数据目录和配置中的环境变量名无需改名。新程序为 nestos-node，启动横幅、MCP serverInfo 与新日志文件使用新名称；MCP 主发现工具为 nestos_node_capabilities，piagent_capabilities 与 fsagent_capabilities 保留为兼容别名。标准发现同时返回 itookit/nestos-node 与旧 itookit/pi-agent 描述。文件／项目协议标识、x-pi-agent-* 授权头、旧 serverId 前缀及 fs-agent.files／fs-agent.bundle 编码保持兼容，已有摘要、历史和绑定继续使用。子进程同时获得 NESTOS_NODE_PROJECT 与旧 PI_AGENT_PROJECT。


### 项目与原生会话搜索

`project_search` 接收 projectId/revision/query/mode（name/path/content）及可选项目相对 directory。发现描述的 fileSearch 仅在隔离执行和 `/usr/bin/rg` 可用时声明；执行使用固定 argv、字面/忽略大小写（支持换行）匹配、只读 pinned 项目视图，无网络和 symlink 跟随，保留嵌套 mount 遮蔽并排除 gitignore、隐藏、私有及原生 home。上限为 100 条、2 MiB 单记录/单文件、16 MiB 扫描输出、10 秒、4 并发；流式解析时先筛选目录再截断结果，超额明确 truncated。fileSearchVersion=2 表示名称与目录范围契约。无匹配与进程/隔离错误分别返回；取消回收子进程。

文件 discovery metadata 与原生 profile 的 `searchKeywords: true` 声明多关键字 AND：横向空白分词、顺序不限、忽略大小写，所有词须命中同一名称/路径、文件行或原生消息。显式换行仍为字面多行片段。文件内容只启动一次 rg，以最长字面词筛选候选，收集器验证全部词后再计入 100 条上限；多词时不按单个词的 rg `--max-count` 提前截断。Codex、Claude Code、DSH 的结构化历史共用同一匹配规则，私有上下文和角色筛选继续生效。

原生运行时以项目/profile 为回收单位：连续空闲且没有访问 5 分钟后卸载，后台每 60 秒做一次内存检查。执行、未解决对话框与在途 driver 引用均阻止卸载；待回答不会自动取消。DriverSlot 保留元数据、授权与操作回执，释放进程/连接后按需重建；历史继续由 harness 原生存储提供。网页关闭不触发中断或删除，重新打开可继续显示尚存的交互。共享运行时中有任一活动会话时保留整个 driver，避免终止其他会话。

执行中的 Codex 最新历史读取原生 `thread/read`，不优先使用尚未写完的 rollout 文件；更早历史与空闲历史保留文件分页。原生回复在有序 stdio 读取点捕获 `eventCursor: {after, eventEpoch}` 并随历史返回，客户端从该边界继续消费，避免长回答刷新、重开或事件缺口修复后重复拼接正文。

目录范围通过 rg 的 `--ignore-file /dev/stdin` 在遍历阶段剪枝，规则随标准输入传入，不落盘、不经过 Shell，也不引入 fd。保留 `/workspace` 为遍历根，避免显式目录参数绕过隐藏、忽略或链接策略；项目自有忽略规则仍优先，输出再次校验目录范围。名称中的通配符按字面转义；隐藏目录、含换行的目录或规则超过 2 MiB 时保留原有遍历与结果过滤。rg 自行选择线程数，查询数量仍由四个工作槽限制。

Codex、Claude Code、DSH 声明 capabilities.search 和 searchPrompts，`harness_session_search` 接收 profileId、可选 projectId/revision、query、mode（title/content/prompt）、archived。经授权列表和原生历史解析读取用户/assistant 文本与命令摘要，不递归扫描 home。最多 20 页列表、每会话 64 页历史、16 MiB、100 条、15 秒、2 并发。结果携带原生 session/turn/item 身份；超限 truncated=true，当前无续页。状态保留 statusDetails.activeFlags 和最近 turn 结果；notLoaded/未知枚举不推断已经完成，独立 CLI 的可读历史不代表本服务有控制权。


### 原生管理与内联附件

Codex 的 `capabilities.checkpointFork` 表示支持经验证的 `harness_fork.lastTurnId`。节点先确认来源回合已终结，再使用 [app-server 检查点 fork](https://learn.chatgpt.com/docs/app-server) 并核对新 thread 的最后回合。成功回执包含 `checkpoint:{sourceSessionId,turnId,verified}`；如果原生端忽略边界或验证无法完成，仍保留已返回的 session 和 committed 回执，`verified:false` 与具体 code 阻止客户端选择该分支，不能伪装成没有创建。旧 `fork` 布尔值只表示原来的 latest 分支能力；其他适配器未声明 checkpointFork 时拒绝 lastTurnId，不按 kind 猜测能力。

原生边界验收可运行 `python3 src/plugin/harness/tests/real_checkpoint_fork.py`（可用 CODEX_BINARY 指定二进制）。脚本使用临时 CODEX_HOME、空工作区与本地拒绝端点生成两个终结失败回合，实际验证指定第一个回合 fork 后只保留该前缀；不读取个人会话或使用真实模型服务。已验收 Codex CLI 0.164.0。此能力不代表原 thread 的条件追加，也不隔离工作区或项目记忆。

Codex rollout 历史的用户项保留 `internal_chat_message_metadata_passthrough.content_item_kinds` 为 `contentItemKinds`，仅在类型和数量与 content 数组对齐时透传，并优先采用同一元数据的原生 turn_id。项目指令、环境和真实用户正文因此可由展示层区分；不按文本内容猜测来源、不删除或改写上下文、不改变发送行为。旧日志或没有此元数据的其他 harness 不补造来源。

Codex 提供 `harness_rename`、`harness_archive`、`harness_unarchive`、`harness_delete`，均要求可写授权并使用 epoch/requestId 回执去重。rename 调用 thread/name/set，不 resume；archive 允许本实例持有且原生状态明确 idle，或原生 notLoaded 历史；后者不表示外部进程已空闲，归档不会停止独立 CLI。归档前核验派生子会话的授权与状态，归档后释放所有权；unarchive 核验恢复的原生 cwd，不 resume、不启动 turn。delete 调用原生 thread/delete，要求受控 idle 或已归档 notLoaded，前置有界核验全部派生子会话的授权和状态；回执返回 deletedSessionIds，原生负责日志及元数据删除，项目文件保留。

`harness_turn.attachments` 接受最多 5 个内联 text/image：UTF-8 文本每个 64 KiB，PNG/JPEG/WebP 图片解码每个 256 KiB，编码内容总计 512 KiB。拒绝非法名称、二进制文本、宿主路径、远程 URL、其他图片格式和超限内容，在启动 turn 前完成验证。MCP JSON body 上限为 2 MiB，以容纳合法附件的 JSON 转义；其他 JSON 路由仍为 512 KiB。


### Claude Code

显式配置已安装的 Claude CLI 和已存在的私有目录；`home` 对应 `CLAUDE_CONFIG_DIR`，不会自动读取未配置的 `~/.claude`。多身份配置的追加式模板见 [profiles/claude.example.toml](profiles/claude.example.toml)，须绑定已有 principal。项目实例复用 ProjectLauncher 和 Bubblewrap 的目录、mounts、网络授权：

```toml
[[harnesses]]
id = "claude"
kind = "claude"
command = "/usr/local/bin/claude"
home = "/srv/nestos-node/claude-home"
projects = true
```

真实验收版本为 Claude Code 2.1.209。控制使用 [Agent SDK 流式输入](https://code.claude.com/docs/en/agent-sdk/streaming-vs-single-mode) 的 stream-json、initialize/control_request/control_response；每个受控会话独立子进程，最多 16 个。新建、恢复、文本/图片附件、中断、工具审批及 AskUserQuestion 转换为统一 harness 接口。使用 manual 权限模式和 stdio 审批通道；不跳过审批。turn 收到相同原生 user UUID 的 replay，或受控当前 turn 的原生 result 后确认回执；超时或断线结果未知，不自动重发。0.4.3 将 HTTP 的 25 秒观察预算与后台原生确认的生命周期分开：turn 的回执接收器持续归原共享 mutation worker 所有，迟到的确认仍能持久写入同一 requestId；独立 operation 查询可读取该事实。控制和管道写入仍有有界等待，取消不等待原提交回执，进程退出会释放等待并保留 unknown；不以管道写入、active 或助手块冒充原生接纳。诊断记录原操作、Session/turn、阶段、错误类别与耗时，不记录正文或凭据。客户端关闭只释放观察，不中断远端 turn；服务端关闭回收进程组和审批请求。

历史只读解析私有 home 的 `projects/*/*.jsonl`，按实际 cwd 核验项目归属，拒绝链接和越界，不修改原生索引。目录列表检查最多 2048 个目录名称、8192 个文件、16 MiB 元信息及 5 秒；历史每次读取 8 MiB 窗口，返回最多 100 项/2 MiB。真实用户 UUID 定义轮次，多条 assistant 消息保留该身份；工具只返回摘要。恢复使用[原生日志绝对路径](https://code.claude.com/docs/en/sessions)，兼容宿主 cwd 与项目沙箱虚拟 cwd 的差异；恢复前核验整份日志的身份及 cwd，日志上限 16 MiB，超限明确失败；历史读取与恢复验证共用 4 个 blocking 工作槽。搜索共用 100 条/16 MiB/15 秒预算及每会话 64 页上限。

独立 CLI 历史可发现，状态为 notLoaded、owned=false；读取不获得控制权，显式 resume 才建立本服务的独立受控进程，不能接管外部正在运行的进程。Claude 当前未声明 fork/rename/archive/unarchive。新建空会话由原生子进程持有，首条消息写入前不保证重启后可发现；原生 JSONL 落盘可能稍晚于 result 事件，历史刷新会获取随后写入的记录。

```bash
cargo test --test claude --test project_watch
cargo build
python3 src/plugin/harness/tests/real_claude.py
```

最后一项为安装真实 CLI 后可选的验收：使用临时 home/项目、本地 Anthropic 协议 peer，验证原生审批写文件、图片、独立 CLI 发现及恢复；不访问个人数据或云模型，不代替桌面视觉验收。

### 项目目录变化

发现元数据的 `directoryEvents: true` 声明项目目录订阅，`events` 描述统一 SSE 入口。projects 订阅携带 projectId/revision，只推送 version/gap/truncated，不包含宿主路径、文件名或正文。Linux inotify 使用 pinned 根与 mounts，不跟随链接，不扫描隐藏目录、原生 home 或被挂载遮蔽的目录；异步等待内核就绪，新增目录与队列溢出重建监听并重新注册等待句柄，溢出报告 gap。

最多同时 16 个目录订阅，关闭流即释放监听，不维护 watchId 租约或 unwatch 操作。建立监听最多覆盖 2048 个目录、50000 项及 2 秒，覆盖不足明确返回 truncated，不能据此声称完整观察。授权与项目 revision 在发送前重新核验，没有原生会话也能观察目录。目录变化不表示外部会话正在运行，也不授予控制权。

Codex/Claude 子进程只继承允许列表内的环境；代理保留 `HTTP_PROXY/HTTPS_PROXY/ALL_PROXY/NO_PROXY` 及各自小写形式，证书保留 `SSL_CERT_FILE/SSL_CERT_DIR`。大小写同时存在时原样传入，由原生客户端决定优先级。不把代理凭据写入日志，不自动覆盖全局代理设置。

## Native usage snapshots

Codex and Claude profiles advertise `capabilities.usage`. The read-only `harness_session_usage` tool returns `{sessionId, scope: "transcript", source: "native-history", revision, complete, observedAt, totals, entries}`. Session history and inspection also include this snapshot as `session.usage`, so viewers need no additional usage poll. Each entry contains its native turn/model/provider identity and disjoint input, cache-read, cache-write and output token buckets. Reasoning tokens are informational and already included in output.

The reader caches file identity and byte offset, processes at most 32 MiB per request, and reconstructs after restart. Partial/oversized records and counter resets mark incomplete results; entry capacity is bounded at 8192. Repeated Codex cumulative snapshots and Claude message frames do not accumulate twice. Failed/cancelled turns retain their recorded usage. Read errors set `usageUnavailable` on the session without blocking history. No prices, currency conversion, billing rules or ledger writes live here.

The transcript scope includes inherited branch history and only the usage records present in the selected native transcript. It is not newly incurred branch spend or a provider bill, and must not be summed across branches as account billing. Consumers choose their own pricing policy and explicitly label estimates.

## 目录共享与多身份授权（0.2）

参考 [config.access.example.toml](config.access.example.toml)。`principals` 为每个用户声明稳定 `id` 和 Bearer 或 Basic 凭据；一个身份当前配置一个凭据，轮换凭据不改变身份。不要混用顶层旧凭据和 `principals`。多身份模式要求稳定 `server_id`。旧单用户配置继续有效，对应稳定身份 `owner`；旧 catalog 中的 owner=0 自动解释为 owner，其他数字身份拒绝迁移。切换到多身份时需要保留 `id="owner"` 才能继续访问原 owner 项目，不自动转交给配置中的第一个用户。

每个 export 是一个目录共享单元，`access` 为服务器上限。`public=true` 允许匿名和已认证用户读取；`[[exports.grants]]` 按 principal 分配 ro/rw，`execute=true` 独立允许执行，缺省为 false。公开权限始终只读，显式非法凭据返回 401，不降级匿名。目录根继续禁止重叠；第一版统一继承共享根权限，不提供组、嵌套覆盖或公开目录内私密子目录例外。文件、SeqFile、项目挂载和进程均校验同一服务端授权，去掉项目 headers 不能扩大用户的 export 权限。

匿名身份只提供文件读取，不能登记项目、执行命令、访问 sync 或原生历史。认证用户的项目绑定和会话是私有的，共享文件目录不共享聊天历史。原生 profile 必须显式绑定 `principal_id`、使用 `projects=true`，不能使用无项目上下文的旧 workspace 控制入口。不同身份的 harness home 必须互不重叠，且均在 exports 之外。只读目录可以显式授予 execute，以 ro 沙箱挂载运行自己的分析会话；rw 本身不授予执行权。模型账号、home 和审批不能扩大外层目录挂载。

能力发现返回当前 principal 与 grant revision。客户端可在所有文件/MCP 请求附带 `x-pi-agent-principal` 和 `x-pi-agent-grant-revision`；服务端发现身份或授权版本不匹配时，在操作前返回 AUTHORIZATION_CHANGED。项目描述返回有效权限，写权限降为只读后仍可读取原项目；旧服务端上限和客户端选择始终取交集。授权快照包含可见目录的物理身份及 profile home 范围，不能仅依赖显示名称作为缓存身份。

ACL 可通过下述本地管理 CLI 修改配置，重启后生效，不提供在线 ACL 管理接口。重启按原有流程停止和回收进程；客户端需重新发现能力并刷新受影响绑定，不将授权版本变化视为可重放写入。同步仍使用一个配置的 principal/namespace，仅对应身份可访问；多身份模式不会把单个 sync namespace 自动开放给所有目录用户。

itookit 的 MCP 编辑器支持 Bearer、Basic 和显式匿名访问；各连接绑定自己的身份与权限快照。此模式支持不同 itookit 实例共享 nestos-node；同一个 NestOS httpd 当前仍是单用户运行时，客户端证书只是准入，不是多租户隔离。


## 目录授权管理 CLI（0.3.2）

目录本身不保存单独的密码。Bearer/Basic 验证用户身份，export 的 grants 决定该身份可以读、写或执行哪些目录。同一身份可访问多个目录，同一目录可向不同身份开放不同权限；`public` 独立开放匿名只读。配置中的 `access` 是共享目录的上限，`execute` 独立于读写权限。

```bash
nestos-node --help
nestos-node access --help
nestos-node access --config /srv/nestos-node/config.toml list
nestos-node access --config /srv/nestos-node/config.toml validate

# Existing single-user credentials become principal owner, retaining existing permissions.
# Preview first; no configuration, lock or backup file is written by --dry-run.
nestos-node access --config /srv/nestos-node/config.toml --dry-run init
nestos-node access --config /srv/nestos-node/config.toml init

# Provision a credential using the operator's existing environment or service EnvironmentFile.
# Example token generation; keep the same value available to the restarted service.
export PI_ALICE_TOKEN="$(openssl rand -hex 32)"
nestos-node access --config /srv/nestos-node/config.toml principal add alice --token-env PI_ALICE_TOKEN
nestos-node access --config /srv/nestos-node/config.toml grant alice project ro
nestos-node access --config /srv/nestos-node/config.toml grant alice project rw --execute

# A new share has no implicit authenticated grants. Paths must exist and cannot overlap exports.
nestos-node access --config /srv/nestos-node/config.toml share add reference /srv/reference ro
nestos-node access --config /srv/nestos-node/config.toml grant alice reference ro
nestos-node access --config /srv/nestos-node/config.toml share set reference --public true
nestos-node access --config /srv/nestos-node/config.toml check anonymous reference read
nestos-node access --config /srv/nestos-node/config.toml check alice reference write

# Remove explicit access; inherited public reads still apply until public is disabled.
nestos-node access --config /srv/nestos-node/config.toml revoke alice reference
nestos-node access --config /srv/nestos-node/config.toml share set reference --public false
nestos-node access --config /srv/nestos-node/config.toml share remove reference

# Rotate credentials without changing principal identity or its project/session ownership.
nestos-node access --config /srv/nestos-node/config.toml principal credential alice --token-env PI_ALICE_NEW_TOKEN
nestos-node access --config /srv/nestos-node/config.toml principal add bob --username bob --password-env PI_BOB_PASSWORD
nestos-node access --config /srv/nestos-node/config.toml principal remove bob
```

`--config` 可放在命令参数前后；省略时沿用服务的 `config.toml` 查找规则。所有输出为 JSON，只有身份、凭据来源字段名和权限，不打印凭据值。拒绝的 `check` 会返回 `allowed: false` 并以非零状态退出；结果明确标记 `scope: configured-policy`、`runtimeVerified: false`，用于配置决策，不代表运行中进程已经应用修改。检查通过只证明认证、目录、原生 profile 等相关配置可被解析，不证明独占写锁、沙箱、模型登录或网络可运行。

环境变量引用会使用当前 CLI 进程的环境进行校验；运行服务也必须能读取这些环境变量。命令不接受明文 token/password 参数，以免凭据落入 shell 历史或进程参数。目录权限按已登记的共享根继承，不能用该 CLI 给同一 export 的子目录覆盖权限；不同访问策略应使用互不重叠的 export。

CLI 分为三层：`cli/` 解析参数和显示结果；`admin/` 编排本地管理、迁移与原子配置事务；`config/inspect` 复用 `config/authorization` 编译器。检查只读打开目录，不创建项目或同步库、不争抢运行中服务的 export 写锁，也不启动 harness。

每次有效修改先完成完整的目录授权校验，再在配置同目录原子替换，并保留 `config.toml.backup-*.toml` 原始备份；新配置与备份权限均为 0600。合作式编辑锁和提交前的原文件核对拒绝并发覆盖；失败不替换原配置。`--dry-run` 只返回预览，不写文件。顶层注释与其他设置保留。导出授权统一写为每个 `[[exports]]` 内的 `grants = [{ principal = "owner", access = "rw", execute = true }]` 数组，避免新增导出后误把后续 `[[exports.grants]]` 归给新目录；旧式嵌套表仍可读取，管理命令保存时会规范化授权数组。共享移除只移除配置，不删除目录文件；被 grants、harness 或启用中的 sync 引用的身份，以及最后一个身份，不可直接删除。

`access init` 保留稳定身份 `owner`、现有凭据、目录上限和执行权限，补齐项目模式原生 profile 的 `principal_id=owner`。原配置缺少稳定节点 ID 时生成并保存一个；已有 ID 不变。存在旧式、非项目模式 harness workspace 时拒绝迁移，需先显式整理其配置。修改完成返回 `restartRequired: true`；重启后客户端重新发现身份和 grant revision。CLI 不自动重启服务，也不修改项目、会话数据。

`prompt` 只匹配 userMessage；原生 `contentItemKinds` 对齐时排除已知注入上下文，未知来源文本保持可见。DSH 与 Codex 共享有界授权列表/历史分页搜索，Claude Code 保留自身的原生 JSONL 授权解析。新增范围由能力声明协商，不在应用宿主退回读取全部原生历史。

## Native harness slash commands

`harness_profiles` advertises `capabilities.slashCommands`. `harness_commands` takes the selected `profileId` and optional `sessionId`, returning a bounded `commands` list with `name`, `description`, `argumentHint`, `kind` (`prompt` or `action`), optional definition `id`, `revision`, and `execution` (`query` or `serial`). Catalog reads do not create a conversation or adopt native execution. Session-dependent catalogs appear after native initialization. Old descriptors without `execution` remain serial.

`harness_command` takes `profileId`, `sessionId`, `command`, optional string `arguments`, plus the existing `epoch` and `requestId`. Project calls retain `projectId` and `revision`. It uses the same authorization, mutation serialization and receipt lookup as other harness writes; unknown results must be reconciled, never replayed with a new request ID. Commands may return no turn, a `turnId`, or a native `commandId` with `output` and `failed`. A committed command with a failed handler remains a committed operation.

Pass `expectedCommand: {id?, revision?}` to bind dispatch to the discovered definition. The node validates the current descriptor before admitting a query and again before execution; changed bindings fail with `HARNESS_COMMAND_CHANGED`. Legacy explicit queries accept no arguments or model override and retain mutation receipts. When `capabilities.commandQueries` is true, use the independent read-only port below instead. `/session:status` reports observed runtime state, exact turn identity when known, `scope` (`controlled` or `external`) and `loaded`; it never adopts an external session. `/status` is an alias only when the native catalog has no command with that name. External history cannot prove execution owned by another process is idle.

Command results may carry `status: running|completed`. Asynchronous compact work emits `harness/command/updated` with command and exact turn identities; accepting the RPC does not finish the work. Serial instruction queues retain their own admission receipt even when later queries run, and wait for completion evidence before advancing. Lost events may be repaired from the exact native turn; an unknown receipt is never replayed.

- Claude: discover the initialized SDK command list, including project/plugin commands and aliases. Cold sessions use a short-lived, nonpersistent SDK process in their verified working directory without adopting the session. Compact and its aliases are actions and may complete without a user-message echo; other prompt commands dispatch the exact slash input. Interactive-terminal-only commands are not synthesized.
- Codex: use `codex app-server` with its default stdio transport. Expose `/compact` through asynchronous `thread/compact/start` and `/review [instructions]` through inline `review/start`. `/models`, `/mcp`, `/skills` query native APIs; discovered `/skill:<name>` commands use typed skill inputs. Duplicate skill names receive path-derived identities and disambiguated command names. Other terminal UI commands are not forwarded as ordinary prompts.
- Claude: preserve compact boundaries, local command output and `session_state_changed` events enabled by `CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS=1`. `/session:models`, `/session:mcp`, `/session:context` query supported SDK controls; unavailable controls return a capability error instead of becoming prompts.
- DSH: use `commands/list` only while an authorized observation is attached and pinned, then call `commands/execute` for serial commands. Native command discovery can resolve and resume an Agent: cold and archived sessions expose only adapter-owned queries, with `observation: {complete: false, code}`; native catalog conflicts retain those queries without claiming a writer. Native command/run and command/done records retain their command identity in history. `/session:models` queries the model catalog. Native descriptors lacking explicit query semantics remain serial; command names alone never grant concurrent execution.

Adding another harness uses its driver-owned catalog and dispatch; the gateway and UI do not maintain a global list of command names.


### Independent command queries

Profiles with `capabilities.commandQueries: true` expose `harness_query`. Supply `profileId`, `sessionId`, an advertised query `command`, correlation `requestId`, optional `expectedCommand: {id?, revision?}`, and the usual project identity/revision. No execution epoch is required. This is a read operation: it does not allocate a mutation receipt, acquire a mutation queue, resume a session, or submit a prompt. Serial commands, meaningful arguments and model overrides are rejected by the adapter. The response is a completed command observation with `commandId` equal to the correlation ID and no `turnId`.

Built-in state queries do not depend on full native command discovery. Clients must negotiate the capability before sending this request; older nodes retain their conservative command/receipt path. A host may save the resulting report in its existing recovery/history store and acknowledge that report independently of input submission. Query-report recovery must not call `harness_operation`; an absent durable report is not evidence about any native execution. Unknown reports never authorize replaying an execution command.

### Native models and reasoning

Profiles advertise `capabilities.models`. Read `harness_models` with `profileId` and optional authorized `sessionId` to obtain `models: [{id, label, reasoningEfforts: [{id, label}], defaultReasoningEffort?}]`, optional `default`, and known `current` selections. Selection IDs are opaque to clients. Missing current metadata does not imply a default was applied. Codex discovery follows bounded native model-list pagination; DSH includes routable providers only; Claude uses SDK model metadata and does not invent effort levels.

Pass `selection: {modelId, reasoningEffort?}` with `harness_turn`. The adapter checks that the model and effort belong together before submitting the prompt. Codex passes model/effort to turn/start; Claude applies set_model and session-only apply_flag_settings before the user frame; DSH applies session/selectModel before session/prompt. The entire operation retains normal requestId/epoch receipts and session serialization. Settings changes do not write user or project configuration files. An unknown result must be reconciled, never automatically replayed.

Claude model discovery without a live session initializes a short-lived SDK process using --no-session-persistence, reads metadata, and drains the process without sending a prompt. Prompt-kind slash commands may carry a selection; native action commands retain their own configuration semantics and reject an explicit selection override.

### 已接纳执行的故障恢复基准

Codex profile 可设置 `[harnesses.codex] reasoning_summary = "auto"`（也接受 concise/detailed/none）。未设置时继承原生偏好；设置后普通输入和 prompt 命令在 `turn/start.summary` 显式覆盖，已有线程下一轮也生效，不改 model、effort 或 service tier，不因原生拒绝而重发。升级 profile.command 的 Codex 二进制只影响下次创建原生运行时；不要为版本对齐中断正在执行的共享运行时。

位于已挂载 harness home 内的可执行文件保留原路径，以便解析同目录辅助程序和资源；这不额外挂载其父目录。其他非系统路径仍只映射单个可执行文件到 `/opt/harness`。

Codex 声明 `capabilities.executionRecovery`。会话历史及 `harness_session_info` 的 session.executionRecovery 返回 running/detached/unknown、checkedAt、runtimeId、processId、childPid、runtimeStartedAt；runtimeStartedAt 是本运行时启动时间，不冒充 /proc 的精确进程启动时间。原生状态 active 或写锁 held 时为 running；仅在 notLoaded、owned=false、loaded=false、没有 activeTurnId 且写锁确认为 not-held 时为 detached，其余为 unknown。它只证明当前控制关系与占用，不是某个 turn 的执行终态。

httpd 可在用户显式确认后，依据新读取的 detached 证据结束旧队首等待，把结果未知及证据保存到原用户恢复 journal，并暂停后续队列。超时只触发检查，不自动 kill、不伪造终态、不重放。正常中断使用精确 sessionId/turnId；Codex app-server 当前可能承载多个会话，强杀其进程组会影响同一运行时的所有会话。保证单会话强杀隔离需要独立的每会话进程生命周期，当前接口不承诺这种隔离。

Codex rollout 回退历史将带明确 `turn_id` 的 `task_complete` / `turn_aborted` 分别投影为 `completed` / `interrupted` 的空消息轮次，与消息页一同返回，供客户端按精确 turn ID 结算。终态记录也计入每页 100 项及 2 MiB 限制；分页不补造缺失终态。正文、普通 error、task_started、会话空闲及无 turn_id 的记录均不证明完成。lastTurnResult 的历史回退只读取与 lastTurnId 匹配的轮次，不能借用页末另一轮的结果。`harness.history.terminals` 日志只记录会话、阶段、终态数量和耗时。

`harness_session_info` 与历史返回的 `session.historyRevision` 由经目录授权打开的 rollout 文件身份、大小及纳秒修改/变更时间生成，不读取正文计算版本。历史先取版本再读内容，避免把并发追加但尚未读到的内容错误确认为已同步。外部 CLI/桌面进程不会向本节点 App Server 的 stdio 广播事件，消费者须用该版本核对外部历史；文件版本不证明 turn 完成或执行权归属。
