//! POSIX process supervision. Requirements verified by the macOS spike:
//! record the descendant snapshot with the stop intent (orphans reparent
//! and leave the process tree once the leader exits), re-verify
//! `pid + start time` before signaling, read process state rather than
//! presence (a zombie still lists but cannot write), and reap children the
//! supervisor spawned.
