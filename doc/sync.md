# 单节点同步存储

同步库使用独立的 `sync.root`，与 export、工作目录和 Bash 执行数据分开。它保存不可变 SHA-256 对象、数据集版本和持久操作回执，支持同一账号的多端设备交替发布。客户端负责文件扫描、共同基线、合并、覆盖和会话接续；本服务负责存储、条件发布、发现和恢复。

## 部署和管理员命令

使用 [纯同步配置](../config.sync.example.toml)，设置凭据和新的空存储目录。`execution=false` 时无需 bubblewrap，也可以不配置 exports。所有同步 HTTP 请求复用现有认证；副本 ID 不能替代认证凭据。

```bash
cargo build --release
target/release/cells /path/to/config.toml
```

首次启动时，若 `sync.root` 不存在、为空，或只剩上次启动留下的 `sync.lock`，服务会自动创建并初始化存储，并记录 `sync.initialized` 事件。`sync init` 保留给显式初始化和无服务启动的部署流程，两者使用同一套拒绝规则：

```bash
target/release/cells sync init /path/to/config.toml
```

自动初始化只覆盖尚无数据库的全新目录。目录内已有其他文件、`storage.json`、对象或 WAL 时，启动不会新建数据库或新身份，而是拒绝：`SYNC_ROOT_NOT_EMPTY`、`SYNC_INITIALIZATION_UNSAFE` 或 `SYNC_INITIALIZATION_INCOMPLETE`。只读 `sync verify` 对未初始化目录返回 `SYNC_NOT_INITIALIZED`，不写入任何内容。误挂载或误指向新目录会得到新的 authorityId，可用 `expected_authority_id` 检出。

同步与执行可以同时启用，但同步库不得与 export、只读运行库及其实际目录别名重叠。启动会拒绝重叠目录，判定在目录创建前完成。数据库和对象库必须位于支持 SQLite WAL、文件锁、硬链接和 fsync 的本地文件系统。

两个 example 是同一个 cells 二进制的部署示例，不是两个不同服务。普通文件、执行和同步可以在一个进程、一个端口上同时提供：在普通配置中增加以下段即可，现有 exports 保留。纯同步示例则明确关闭 execution 并省略 exports。若选择启动两个进程，必须使用不同 listen 地址或端口；同一 sync.root 只能由一个实例独占打开。

```toml
[sync]
enabled = true
root = "/srv/cells-sync"
```

除通用监听与认证配置外，sync 只需显式启用并配置私有存储路径。其余参数都有内置默认值；示例不再重复全部默认值。默认对象容量预算为 10 GiB、单对象上限 256 MiB、历史与回收窗口 30 天、changes 窗口 7 天，适合先采用默认配置再按实际容量调整。principal_id 与 namespace_id 默认分别为 owner、personal，它们是单账号存储身份，不是由 exports 的目录名派生；启用后的有效限额可通过同步 capabilities 查询。

### 项目标识与本地目录映射

普通 export 通过 alias 区分，默认 alias 来自目录最后一层名称，也可以显式覆盖。sync 则通过 namespaceId、projectId、datasetId 区分，使用 manifest 内的相对路径保存文件树；不按本地目录末级名称自动识别项目，也不把客户端绝对路径当作云端身份。

一个 itookit 项目对应一个本地工作目录，云端对应一个独立项目目录。不同项目分别创建 projectId，分别存储，不能因为末级目录同名而自动合并。只有用户明确选择拉取或绑定同一个云端项目时，多个设备的本地工作目录才对应同一云端 projectId。项目中的 src/main.ts、docs/guide.md 等相对路径完整保留；客户端选择的本地根目录及其上级路径不需要上传，本地目录改名不改变项目身份。

当前正式对象已经按项目物理分目录，而不是把不同项目的内容混在一个全局对象目录：

```text
sync.root/
├── metadata.db
└── objects/personal/
    ├── project-A/    # Content objects belonging to project A
    └── project-B/    # Content objects belonging to project B
```

project-A 与 project-B 内分别保存该项目的文件、会话和附件对象；相同摘要也不跨项目合并。元数据库、上传暂存及服务锁由实例共享，元数据查询和变更按项目作用域隔离。配置只指定一个 sync.root，项目目录由服务自动管理，无需逐项目增加 TOML 配置。

服务端对象实际位于 sync.root/objects/namespaceId/projectId/hash 前缀/hash，并非可直接浏览编辑的项目目录镜像。用户选择本地根目录与云端项目的绑定应由 itookit 客户端管理；当前 cells 已提供项目/数据集协议，尚未交付 itookit 的目录绑定 UI 或项目同步客户端。路径式云端展示可以作为客户端导航，但不是当前协议中的服务器目录映射接口。

管理员操作需要停机，并使用同一个 `sync.lock`；另一实例持锁时命令失败。备份和恢复目的目录必须为空，且不能与源目录重叠。

```bash
target/release/cells sync verify /path/to/config.toml
target/release/cells sync gc /path/to/config.toml
target/release/cells sync backup /path/to/config.toml /independent-disk/backup-001
# 配置中的 sync.root 应改为新的空目标目录。
target/release/cells sync restore /path/to/restore.toml /independent-disk/backup-001
# 使用经过完整摘要校验的源文件修复指定对象。
target/release/cells sync repair /path/to/config.toml PROJECT SHA256 /path/to/trusted-object
```

备份包含静止的数据库、仍存在的 WAL、对象、每个文件的完整性清单和最后写入的 `COMPLETE` 标记。校验使用独立副本，不修改最终备份。备份不自动打包认证配置、TLS 或部署凭据，应另行保存这些配置。用于防磁盘损坏的备份应存放在独立故障域。

恢复保持 authorityId，生成新的 historyEpoch 和 cursor 密钥。旧操作记录留作诊断，新代次不继承旧副本序号。客户端保留本地修改，重新注册副本、读取成员和 head、对账并激活，从 opSeq=1 开始发布。当前管理员命令提供同服务灾备恢复；另一个 authority 的克隆需后续增加明确入口。

初始化中断产生 `init.pending`，可重复执行同一 init 命令。恢复中断产生 `restore.pending`，普通启动拒绝打开它；可用原备份重复执行 restore。修复与 GC 的持久意图由正常启动恢复。已有对象、标记或 WAL 而数据库缺失时，服务拒绝启动：自动初始化只在真正没有数据库的全新目录上运行，不会把残缺存储替换成新身份。

init 续作也遵守上述拒绝规则：已有 storage.json、对象或 WAL 时，init.pending 不能授权创建空数据库或新身份。数据库身份已写入但 marker 尚未完成时，可以沿用原身份完成首次初始化。

verify 使用只读数据库连接，不执行启动恢复、不重建引用缓存、不登记 corrupt；它检查数据库、对象摘要和受保护 manifest 的引用闭包。损坏修复由 repair 显式执行，未完成的初始化/恢复应先完成对应流程。直接调用 backup 会停止该实例准入并等待活动任务，然后在数据库锁保护下复制；管理员 CLI 始终另取 root 独占锁。

## HTTP 使用顺序

路由位于 `/v1/sync`。只有 capabilities 豁免 `X-Sync-History-Epoch`；其余请求携带该头，命令体中的 historyEpoch 必须与头一致。旧代次的读取、查询、取消和写入全部拒绝。

1. `GET /capabilities` 获取 authorityId、historyEpoch、namespace、限制和支持的能力。
2. `POST /replicas` 注册 `{ "replicaId": "device-A" }`，得到 reconciling 状态。
3. `GET /projects?state=all`，再按选择范围读取 `GET /projects/P/datasets?state=all`；有 nextCursor 时继续分页。
4. `POST /replicas/device-A/activate`，提交 `scopes: [{projectId, cursor}]`。没有项目时可以提交空 scopes；已有项目时至少选择一个有效清单范围。
5. 创建项目、检查并上传对象、创建数据集；后续更新使用当前 head 条件发布。

注册、激活、pin 和对象上传采用各自身份契约；项目及数据集变更统一携带 operationId、replicaId、opSeq、authorityId、historyEpoch。项目范围内的变更另需 expectedProjectLifecycleRevision。opSeq 和 generation 均为规范十进制字符串，不使用浮点数。

```json
{
  "operationId": "device-A-3",
  "replicaId": "device-A",
  "opSeq": "3",
  "authorityId": "<capabilities authorityId>",
  "historyEpoch": "<capabilities historyEpoch>",
  "expectedProjectLifecycleRevision": "1",
  "expectedHead": { "generation": "1", "manifestHash": "<current hash>" },
  "nextManifestHash": "<uploaded canonical manifest hash>"
}
```

创建项目使用 `POST /projects` 和 projectId。创建数据集使用 `POST /projects/P/datasets`，增加 datasetId、kind、logicalId、manifestHash，首个 head 与成员事件一同提交。kind 支持 files、session、organization、definitions、bundle；files 使用文件 manifest，其余使用通用 bundle。项目和逻辑身份删除后不复用。

### 对象与 manifest

`POST /projects/P/objects/check` 接受最多 1000 个 hashes，仅返回当前项目 ready 对象。`PUT /projects/P/objects/HASH` 传原始字节，必须提供 Content-Length；采用整对象传输，无块级续传。对已验证存在的对象可以直接复用并返回 reused，避免满额时重复占用配额。新的上传流计算并验证完整摘要，超过长度、对象预算、期限或并发预算时拒绝。

manifest 通过同一 PUT 接口上传，使用 JCS 的受限 schema：固定 ASCII 字段名、整数 version=1、规范字符串及十进制 size，拒绝扩展字段。文件 entries 按路径 UTF-8 字节序排序；父目录显式存在，根目录隐含。服务端在发布时验证规范编码、对象长度及闭包。

```json
{"entries":[],"format":"fs-agent.files","version":1}
```

`GET /projects/P/objects/HASH` 支持单 Range、If-Range 和 ETag，下载前完整校验服务端对象，并保留打开的文件描述符直到响应结束。客户端仍需在全量接收或 Range 拼接后校验完整 SHA-256。`GET /projects/P/manifests/HASH` 只返回已经发布校验过的 manifest。

### 发布、发现和回执

`POST /projects/P/datasets/D/publish` 使用上面的条件请求。成功返回 committed 和新 head；同 manifest 返回 noChange，仍需满足旧 head 条件。竞争失败返回 HEAD_CONFLICT 和 not-committed，消费该 opSeq，重规划使用下一序号。

同副本、同序号、同规范命令返回原回执；同序号不同内容返回 OPERATION_REUSED。`GET /replicas/R/operations/SEQ` 查询持久结果。回执过期后返回 OPERATION_EXPIRED；保留高水位，不把旧序号作为新命令。`POST /replicas/R/operations/SEQ/cancel` 在原请求未准入时需携带 `{target, command}`，其中 command 是完整原请求；已提交结果不能取消或回滚。

进入提交阶段后，取消仅返回当前结果，客户端可以停止等待，但须通过查询确认终态。首版不承诺中断所有 pending 发布。

成员枚举返回固定 catalogRevision、cursor、nextCursor 和 changesCursor。`GET /projects/P/changes?cursor=...` 返回固定上界分页事件及 hasMore；发现到的是已发布版本，无法发现其他设备尚未上传的数据。游标签名绑定 namespace、项目、过滤条件、代次与期限，过期后全量对账。成员列表可以发现新建的 session 数据集，但服务器不合并 Round 或恢复旧运行。

`POST /projects/P/replicas/R/ack` 携带 cursor 和规范十进制 scopeRevision。ACK 检查副本活跃状态、写入门禁和新增行预算；范围 revision 不得回退，同一范围 revision 的消费位置也不得回退。它不代表客户端共同基线或磁盘应用已经完成。

### 历史、删除与保护

`GET /projects/P/datasets/D/versions` 与 changes 独立，支持 cursor/limit；`.../versions/G` 查询指定版本。响应包含 generation、manifestHash、committedAt、supersededAt、retainUntil 和 contentStatus。选择旧 manifest 后正常 publish，H8 恢复 H3 内容会产生 H9。

`POST /projects/P/datasets/D/delete` 携带 expectedHead；restore 携带 expectedDeletedGeneration、sourceGeneration 和项目生命周期条件。`POST /projects/P/delete` 固定当时有效成员；`.../restore` 撤销项目删除，任何必要对象损坏则整体拒绝。删除与恢复均推进项目 lifecycleRevision，旧写入计划不能复活。项目撤销删除不等于任意时间点整体回滚，projectCheckpoint=false。

旧 head 从被替代时起获得历史窗口，删除内容从删除时起获得回收窗口；当前有效 head 永远保护。对象保留根包括当前 head、已登记的历史/回收期限、回执期限和读取 pin。多个引用取保护并集，修改保留配置不缩短既有承诺。

`POST /projects/P/read-pins` 接受 manifestHash、requestKey、可选 ttlSeconds。renew/release 使用返回 pinId；读取保护到期后需要重建。release 可提前释放记录，对象可保守保留至已经承诺的期限。

服务每分钟执行一次有界 GC；管理员 gc 可立即运行。每批最多删除 1000 个对象，通过 deleting 状态和 gc_items 恢复未完成删除。运行期使用单调时间推进期限；重启发现时钟回退时暂停 GC。

配额按同项目去重后的已安装对象与上传预留计算，跨项目分别计费。当前、历史和暂存均占额度，不因容量满提前删历史。保留数据库工作空间；无法确认的存储错误返回 unknown 并关闭写入准入，查询持久结果后修复或重启。

准入在数据库锁内复核。关闭中拒绝新外部写入，已接受命令和已取得预留的上传可受控收尾；提交结果不确定后禁止继续提交。读取 I/O 或普通查询失败不会直接把对象标成 corrupt，也不一律关闭全库；已存内容摘要/长度不符和缺失才支持持久损坏判断。SQL 查询失败向上传播，不伪装成版本 expired；新上传摘要不符属于输入错误。

操作准入预留 operation-id 与 pending 回执行，最终业务新增行超限则回滚业务并更新既有拒绝回执。确定性拒绝只有在外层事务提交后才可返回终态；SQL 自动回滚、SAVEPOINT 清理失败或回执写失败返回 unknown，保留准入记录供恢复。逻辑行预算与真实磁盘故障分别测试，磁盘保留预算不等于永远能写成功。

drain 只在停止准入后调用，跟踪接收中的上传、blocking 工作、后台变更和异步清理；超时明确返回 SYNC_DRAIN_TIMEOUT。进程记录 sync.drain_failed 后按退出期限结束，剩余意图由重启恢复，不能将超时当作成功排空。

changes 与重复 catalog 快照按窗口压缩；历史版本目录及身份记录仍保守保留，达到 max_metadata_records 时拒绝增长。该限额同时覆盖 records、对象、manifest 引用和上传记录。副本过期后不得原地激活，应注册新副本并对账。正文可回收，身份 tombstone 和序号高水位独立保留。

## 实现与验收

SQLite 使用 WAL、synchronous=FULL、foreign_keys=ON。项目、数据集、版本、成员快照、副本和操作使用带 scope/kind/key 主键的类型化 records；对象、manifest_refs、上传预留和 gc_items 使用独立关系表。领域规则在 policy/commands/operations/catalog/retention，文件机制在 store，HTTP 只解析和传输。

```bash
cargo fmt --check
cargo test --all-features -- --test-threads=1
# 新版 clippy 的 manual_inspect 告警来自现有两个非同步模块。
cargo clippy --all-targets --all-features -- -D warnings -A clippy::manual_inspect
CELLS_PROCESS_TEST=1 cargo test --all-features -- --test-threads=1
node scripts/check-sync-fixtures.mjs
cargo build
python3 scripts/sync-smoke.py target/debug/cells
```

sync-fault-injection 是显式测试 feature，默认构建不读取故障环境变量。它覆盖准入、发布提交、安装、GC、修复和恢复中断，以及回滚/回执写失败和提交已成功但结果不确定的分类。故障子进程测试串行运行，避免 fork 期间暂时继承其他测试的 root 锁；CAS 测试仍显式创建竞争线程。库内正确性测试另包含实际 SQLite FULL、锁内门禁、只读 verify 和流式关闭/取消。真实 HTTP 脚本包含 A/B/C 条件发布、回执去重、原 root 不可用时的空目录恢复、旧 epoch 查询/取消/重试隔离及受保护版本的摘要校验。进程退出和逻辑 I/O 故障不代表真实断电验收。


## 发现索引压缩与性能诊断

`change_retention_seconds` 默认 604800 秒，必须不小于 `read_pin_seconds`。GC 仅压缩带有可靠 recordedAt 的连续事件前缀，实际回收阈值为事件时间加 changes 保留窗口，再加一个完整游标 TTL。旧存储中缺少时间戳的事件形成保守屏障，不猜测提交时间。历史内容、历史版本目录、副本身份、operation-id 和删除标记的保留规则不受该设置影响。

每次 GC 的发现索引清理最多删除 2000 行，单项目每类最多 1000 行。项目 changeFloor、事件删除和 catalog 压缩在同一事务提交。catalog 保留边界及之前每个数据集最后一份快照，包括已删除成员，并保留边界之后全部快照。正常分页的原到期时间保持不变；游标低于清理边界时，catalog、changes、ACK 和 activate 都返回 CURSOR_EXPIRED，客户端重新枚举成员、全量对账。没有 cursor 的 changes 请求在前缀已经清理后同样返回该错误，不能静默提供不完整日志。

`SyncService::diagnostics()` 提供进程内累计次数、纳秒总量和最大值：lockWait、lockHold、transaction、commit。transaction 包含 BEGIN 到 COMMIT 或错误回滚；commit 单列提交耗时。正常维护每分钟在 debug 日志中输出 sync.diagnostics。指标为近似并发采样，重启清零，不增加 HTTP 管理入口。

可重复的局部负载入口：

```bash
cargo test --all-features --offline --lib sync_publish_diagnostics -- --ignored --nocapture --test-threads=1
```

该负载使用两个约 124 KB 的 manifest、1000 个文件路径共享同一内容对象、100 次真实版本发布与 4 个并发读取线程。发布按唯一摘要检查和保存引用，同摘要不同长度的清单拒绝；不会改变 head/版本/事件/回执的原子事务。它专门测量共享对象场景，不代表大量独有对象、大文件上传或生产磁盘的性能。

历史摘要及防重放身份仍按原契约保留，因此总元数据并非无限可增长。配额达到上限时仍拒绝新增长；本次没有通过清理身份记录来放宽防重放保证，也没有执行自动 VACUUM 压缩数据库文件。
