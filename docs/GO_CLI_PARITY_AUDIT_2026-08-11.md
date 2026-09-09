# Go kcptun CLI / 功能对齐审计与修复报告

审计日期：2026-08-11

- Go 基线：`kcptun-go`，commit `42eb6b012e61`
- Rust 基线：本仓库 commit `76335f473ad5` 加本次工作区修复
- 审计范围：client/server CLI、JSON 配置覆盖、运行时行为、日志、Tokio/Smol、Go 线协议互操作

## 结论

本次发现的高、中风险功能差异已经修复。参数不再只是“能够被 clap
解析”，而是已沿调用链检查到实际运行位置，并用单元、集成、压力和
Go↔Rust 互操作测试验证。

`--autoexpire` 的 Go 语义特别容易误解：它不是空闲超时，也不会由后台
定时器主动重连。session 到达 `creation + autoexpire` 后，只有下一条本地
连接轮询到该槽位时才创建新 session；旧 session 在
`creation + autoexpire + scavengettl` 后由 5 秒周期 scavenger 强制关闭。
因此没有新的本地连接时，看不到 reconnect 日志是正常的。

## 已修复的问题

| 范围 | 修复前 | 修复后 / Go 对齐结果 |
|---|---|---|
| `--autoexpire` | 曾按 activity 理解；被替换 session 不在 scavenger 列表 | 使用绝对 creation time；维护历史 session 列表；TTL 到期日志与 Go 语义一致 |
| `--tcp --conn N` | tcpraw 只创建一个 session，第二条本地连接可能越界 panic | 创建 N 个独立 session，连接池长度与轮询模数一致 |
| `--remoteaddr` 端口范围 | 固定按槽位选择端口 | 每次初建/重连使用安全随机数选择端口，并重新解析 DNS |
| `--scavengettl` | session 被替换后失去可关闭引用 | 历史 session 保留到正常关闭或 TTL scavenging |
| `--closewait` | 等双向 EOF 后再 sleep，单边 EOF 可永久挂住 | 任一 copy 完成即启动该方向 grace；到期关闭双方，匹配 Go `std.Pipe` |
| `--framesize` | 仅校验/保存，实际出站仍固定 60000 | SMUX 实际按配置值拆帧；新增 1024-byte 分帧测试 |
| SMUX 校验 | 存在 Rust 自定义下限；keepalive timeout=`3×interval` | version/frame/buffer/keepalive 校验按 Go；timeout 固定 30 秒 |
| JSON 配置 | 布尔值使用 OR，JSON `false` 不能覆盖 CLI `true`；拒绝未知字段 | JSON 最后覆盖 CLI，包括显式 `false`；未知字段忽略 |
| `-ds` / `-ps` | clap 不接受 Go 的单横线多字符 alias | 启动前规范化为 `--datashard` / `--parityshard` |
| `--ratelimit` | 在 SMUX/压缩层按上层字节计数，burst 不同 | 固定 96000-byte burst；在 FEC、加密之后按实际上线字节限速，ACK 也计入 |
| `--snmplog` | UTC/strftime、错误 CSV header、首条数据晚一个周期 | 支持 Go 时间 layout 和本地时区；header 为 `Unix`；首周期写 header+data |
| SIGUSR1 SNMP | handler 置位但无人消费 | 后台消费信号并立即输出 SNMP 快照 |
| SNMP session 计数 | client 重连只增不减 | session 构造成功后计数，Drop/关闭时递减 |
| `--QPPCount` 校验 | seed/pad 阈值和 gcd 基数错误 | 对齐 Go：seed 211、pads 7、与 8 互质；smoke 参数名同步 |
| `--pprof` | 标准 Rust 构建没有功能，flag 只能警告 | client/server 默认 Tokio、标准 Smol、full ARM 构建包含 pprof |
| client `--localaddr` | 只支持 TCP | Unix 平台支持 Unix-domain listener，退出时清理 socket 文件 |
| server `--target` | 只支持 TCP | TCP 地址解析失败时按 Unix socket path 连接，Tokio/Smol 均支持 |
| multiport `:0` | Rust 接受 | 与 Go 一致拒绝 0 端口 |
| autoexpire 测试 | 仍断言旧 activity 语义和旧日志 | 改为 creation-time、随机端口和当前 TTL 日志断言 |

## 参数逐组核对结果

### client 专属

| 参数 | 结果 |
|---|---|
| `localaddr/-l` | TCP 与 Unix path 均生效 |
| `remoteaddr/-r` | 单端口、范围、DNS、每次随机选择均生效 |
| `conn` | 必须大于 0；UDP/tcpraw 均建立对应数量 session |
| `autoexpire` | 按创建时间生效；下一条本地连接触发 replacement |
| `scavengettl` | 对当前和已替换的历史 session 均生效 |

### server 专属

| 参数 | 结果 |
|---|---|
| `listen/-l` | 单端口/范围 UDP 生效；`--tcp` 额外开启 tcpraw，不关闭 UDP |
| `target/-t` | TCP target 与 Unix socket target 均生效 |

### client/server 公共

下列参数均已从 CLI/JSON 追踪到运行时消费者，并在适用处通过 Go 互操作
矩阵验证：

- 协议/加密：`key`、`crypt`、`QPP`、`QPPCount`、`mode`、`mtu`
- KCP：`sndwnd`、`rcvwnd`、`datashard/-ds`、`parityshard/-ps`、
  `acknodelay`、`nodelay`、`interval`、`resend`、`nc`
- SMUX：`smuxver`、`smuxbuf`、`streambuf`、`framesize`、`keepalive`
- 传输：`ratelimit`、`dscp`、`sockbuf`、`tcp`
- stream：`nocomp`、`closewait`、`quiet`
- 观测：`snmplog`、`snmpperiod`、`pprof`、`log`
- 配置：`-c`（JSON 的标量和布尔值最终覆盖 CLI；未知字段向前兼容）

Rust 对负数、整数溢出等输入仍优先采用强类型解析并直接报错，而 Go 的
若干 `int -> uint16/uint32` 路径会截断或在运行期 panic。这里保留 Rust 的
安全行为，不复制 Go 的已知不安全边界行为；有效值范围内功能一致。

## 日志说明：为什么可能看不到重连

以 `--autoexpire 60 --scavengettl 600` 为例：

1. session 创建后的 60 秒内，即使有流量，也不会延长 expiry；
2. 60 秒后如果没有新的本地 TCP/Unix 连接，client 不会重连；
3. 下一条本地连接到来且轮询到该 session 槽时，才会看到
   `connection N is dead, reconnecting ...` 和 `connection N reconnected`；
4. 旧 session 最早约在创建后 660 秒、再加最多 5 秒扫描误差时，出现
   `scavenger: session closed due to ttl`；
5. 生命周期日志是 `info` 级别。`RUST_LOG=warn` 或 `error` 会隐藏它们，
   应使用默认 `info` 或 `RUST_LOG=info`。

## 验证结果

- `make gate`：通过（format、workspace tests、clippy `-D warnings`）
- `make clippy-smol`：通过
- `make test-smol`：通过，包含 Smol 压力测试
- Unix socket 定向测试：Tokio、Smol 均通过
- closewait 单边 half-close 定向测试：Tokio、Smol 均通过
- 加密后线上字节 ratelimit 定向测试：通过
- `make e2e`：`138 passed, 0 failed, 0 skipped`
- tcpraw e2e：当前 macOS 环境按脚本跳过；该项需要 Linux root

## 保留差异

- Rust 使用 `RUST_LOG` 和 `env_logger`，Go 使用标准库 logger；日志格式和
  level 控制不要求逐字符一致。
- Rust 对非法负数/溢出输入会在 CLI 解析期安全失败，不复刻 Go 的截断、
  ticker panic 或模零 panic。
- tcpraw 行为已做静态路径对齐，但本次 macOS 无法完成 Linux root 动态验证。
