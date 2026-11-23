//! 纯核：无 I/O、无时钟、无随机、无环境变量、无全局状态。
//!
//! 只承载可独立验证的计算：区间代数与几何换算、分区表编解码、文件系统档案、
//! 证据三态与持久记录。所有对外类型都可通过纯函数构造与断言。

pub mod evidence;
pub mod fingerprint;
pub mod geometry;
pub mod policy;
pub mod record;
pub mod region;
pub mod table;
