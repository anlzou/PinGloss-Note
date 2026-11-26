//! Nomo 作为 PingGloss `notes` 插件跑数据面时用的**协议地基**。
//!
//! 这一层是宿主那侧 `pg-plugin` / `pg-plugin-client` 的**原生复刻**，不是依赖它们：R-5 不许把两棵
//! 依赖树并起来，G-9 说 SDK 还没发布，而 `pg-plugin` 会连带拉进 `pg-core`（贴图协议那一面）。于是
//! Nomo 只用手上已有的 `serde` / `serde_json` 把同一份**线上合同**重写一遍 —— 兼容判据是字节级：
//! 键序、`v`、`op`⇄`ok` 的分界、缺 `payload` 与多余顶层位的拒绝、握手那两条规则，都得与宿主对齐。
//!
//! 分档（对应重构文档 §9 的 P1）：
//! - [`frame`]：一行一帧、1 MiB 上限（P1-a）。
//! - [`handshake`]：`Hello`／`HelloOk` 字段与两条拒绝规则（P1-a）。
//! - [`envelope`]：数据面一帧的codec（请求／成形答复／失败答复三形，P1-a）。
//! - [`dispatch`]：处理器结果↔线上答复帧、`Step`／`HostReply`（P1-a）。
//! - [`endpoints`]：读 spawn 那两位 env，判「是不是插件模式」（P1-a）。
//! - [`transport`]：Unix socket／Windows named pipe 上的字节管道（`Listener`／`Stream`，P1-c）。
//! - [`session`]：绑数据面 + 握手 + 逐帧 `pump` + 向宿主 `request`（P1-c）。
//! - [`storage`]：`notes.db` 与 `folders/<dir>/notes/<id>.md` 那套数据口径（P1-b）。
//! - [`runtime`]：插件模式那一格的运行——先握手后建窗、一条连接一个泵线程、把 op 结果递给 UI（P1-c）。

pub(crate) mod attachments;
pub mod dispatch;
pub mod endpoints;
pub mod envelope;
pub mod frame;
pub mod handshake;
pub mod runtime;
pub mod session;
pub mod storage;
pub mod transport;
