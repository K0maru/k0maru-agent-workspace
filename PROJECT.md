# K0maru-Agent-Workspace: Git Worktree 隔离与沙箱执行中枢

## 🎯 项目愿景与使命
作为 **K0maru-Harness** 独立模块化矩阵中的第三块核心积木，构建轻量、零额外运行时开销的智能体工作区隔离与沙箱管理引擎。与 `k0maru-agent-memory` 和 `k0maru-agent-router` 无缝协同，彻底解决并发 Agent 间的分支脏写冲突、物理工作区文件污染与临时执行环境生命周期自动化回收问题。

## 🧩 核心功能规约
1. **Git Worktree 原生隔离**：基于 `git worktree add .worktrees/task-xxx -b feat/task-xxx` 创建独立沙箱，毫秒级就绪且零全量克隆开销。
2. **瞬态生命周期管理**：提供创建（provision）、归档/合并（teardown）、超时巡检清理（prune_stale）机制。
3. **缓存共享链接**：支持 Cargo `target/`、Node `node_modules` 智能复用与软链接，避免重复下载几百兆依赖。
4. **Router 协议联动**：接收 TaskEnvelope 自动配发 Worktree，并将隔离绝对路径传回 Herdr/Tmux 窗格。

## 🧭 研发任务清单 (Phase 1)
- [x] 1. 实现 `WorktreeOrchestrator`：执行底层 `git worktree` 原语，支持任务隔离创建与幂等检查；
- [x] 2. 实现原子清理与合并：支持 Fast-Forward merge、worktree remove、branch -D 回滚；
- [x] 3. 实现 CLI 子命令：`provision`, `teardown`, `list`, `prune`；
- [x] 4. 编写 100% 覆盖率的自动化回归测试与沙箱生命周期测试。
