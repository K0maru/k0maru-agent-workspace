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

## 🧭 研发任务清单 (Phase 2 - Cache & Toolchain Linker)
- [x] 1. 实现 `EnvironmentManager`：支持 Cargo `target/`、Node `node_modules`、Python `.venv` 智能符号链接挂载；
- [x] 2. 实现敏感环境变量与配置注入：支持自动隔离复制 `.env`, `.env.local`, `.mcp.json`，确保沙箱内修改不污染主仓；
- [x] 3. 增强原子清理契约：在沙箱删除与回滚前显式安全解除挂载（`cleanup_mounts`），严防误删主仓共享缓存目录；
- [x] 4. CLI 拓展：`provision` 支持 `--cache <cargo|node|python>` 与 `--inject <file>` 参数；
- [x] 5. 编写 Phase 2 缓存挂载、配置注入与安全回滚的完整集成测试套件。

## 🧭 研发任务清单 (Phase 3 - Router & Multiplexer Adapter)
- [x] 1. 实现 `WorkspaceRouterAdapter`：支持接收 `agent-task-v1` 标准 `TaskEnvelope` 自动配发物理隔离工作区；
- [x] 2. 实现终端多路复用器（Tmux / Herdr）绑定：支持自动生成分屏指令与 Agent（`agy` / `claude-code`）引导命令；
- [x] 3. CLI 拓展：新增 `route` 子命令，支持从 JSON 文件/字符串派发沙箱与窗格；
- [x] 4. 编写 Phase 3 端到端跨仓工单路由与多路复用器切分测试套件。

## 🧭 研发任务清单 (Phase 4 - Deterministic Verifier & Automated Merge Gate)
- [x] 1. 实现 `WorkspaceVerifier`：在沙箱 CWD 隔离运行 build, test, linter 与 scope guard 校验；
- [x] 2. 实现不可变执行凭证 `ExecutionReceipt`：遵循 `agent-receipt-v1.json` 规约，计算 SHA-256 patch 摘要；
- [x] 3. 实现自动化合并与安全回滚门禁：通过时结合 `--auto-merge` 原子合入目标分支，失败结合 `--auto-rollback` 干净回滚；
- [x] 4. CLI 拓展：新增 `verify` 子命令，支持人机可读输出与 `--json` 机器执行凭证；
- [x] 5. 编写 Phase 4 完整单元测试与端到端 CLI 集成测试套件。

## 🧭 研发任务清单 (Phase 5 - Multi-Strategy Isolation & In-Place Passthrough)
- [x] 1. 扩充 `SandboxHandle` 结构：引入 `strategy: IsolationStrategy`，确保 `WorkspaceProvisionRequest` 策略无损透传；
- [x] 2. 实现 `IsolationStrategy::InPlace` 原地穿透：动态发现分支、零 Worktree 与无新分支开销、Teardown 安全空操作（绝对不删 repo_root）；
- [x] 3. 实现 `IsolationStrategy::TempDir` 临时目录隔离：利用 OS 临时目录瞬态就绪、支持依赖挂载、Teardown 自动干净移除；
- [x] 4. 智能策略推断与路由适配：`WorkspaceRouterAdapter` 识别 `read_only`、`in_place`、`query` 标签自动推断 InPlace 策略，支持显式覆盖；
- [x] 5. CLI 拓展：`provision` 与 `route` 子命令支持 `--strategy <worktree|inplace|tempdir>`；
- [x] 6. 编写 Phase 5 完整单元测试与端到端集成测试套件。

