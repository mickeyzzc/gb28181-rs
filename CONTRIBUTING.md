# Contributing / 贡献指南

**English** | [中文](#中文)

## Development process: strict TDD

All changes to this repository follow test-driven development:

1. **Red** — write a failing test that pins the behavior you are about to change (wire-format bytes, boundary conditions, error paths).
2. **Green** — make the minimal change to pass it.
3. **Refactor** — clean up with the test as a safety net.

Tests are written **together with** the code, never backfilled later. A PR that changes behavior without accompanying tests will not be reviewed. Wire-format tests (SOAP/XML/SIP bytes, XML element names) are load-bearing — treat golden strings as contracts.

## Ground rules

- `main` is protected: PR-only merges, CI must pass, no force pushes. One approval is not required (self-merge after green CI is allowed), but the PR must describe the change and its tests.
- Keep commits semantic: `feat:`, `fix:`, `refactor:`, `test:`, `docs:`, `chore:`.
- Do not commit `AGENTS.md` or any local agent/tool configuration — it is gitignored on purpose.
- Temporary artifacts go into `tmp/` (gitignored), never into the source tree.
- Breaking API changes need a major version bump and a migration note in the PR description.

## Verification before pushing

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

CI runs exactly these three gates on every push and PR.

---

## Releasing

1. Merge changes to `main` through a PR (CI must pass).
2. Bump `version` in `Cargo.toml` (semver: patch for fixes, minor for features).
3. Tag the release commit: `git tag vX.Y.Z && git push origin vX.Y.Z`.
   The `release` workflow verifies the tag matches `Cargo.toml`, sits on
   `main`, runs the tests, and publishes to crates.io automatically.
4. Consumers bump their dependency version at their own pace.

## 中文

## 开发模式：严格 TDD

本仓库的一切改动遵循测试驱动开发：

1. **红** —— 先写一个失败的测试，钉住你要改的行为（线上报文字节、边界条件、错误路径）。
2. **绿** —— 做最小改动让它通过。
3. **重构** —— 在测试保护下清理。

测试与代码**同步编写**，禁止事后补齐。没有伴随测试的行为变更 PR 不予评审。线上格式测试（SOAP/XML/SIP 字节、XML 元素名）是承重结构——把黄金字符串当作契约对待。

## 基本规则

- `main` 受保护：仅 PR 合入、CI 必过、禁止 force push。不强制他人审批（CI 绿后可自合并），但 PR 必须说明变更内容与对应测试。
- 提交信息语义化：`feat:`、`fix:`、`refactor:`、`test:`、`docs:`、`chore:`。
- **禁止提交 `AGENTS.md`** 及任何本地代理/工具配置——它已被刻意 gitignore。
- 临时产物一律放 `tmp/`（已 gitignore），不得进入源码树。
- 破坏性 API 变更需升主版本号，并在 PR 描述中附迁移说明。

## 推送前的本地验证

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

CI 在每次 push 与 PR 上精确执行以上三道门。

## 发布流程

1. 变更通过 PR 合入 `main`（CI 必须通过）。
2. 在 `Cargo.toml` 中提升 `version`（语义化：修复提 patch，功能提 minor）。
3. 给发布提交打 tag：`git tag vX.Y.Z && git push origin vX.Y.Z`。
   `release` workflow 会校验 tag 与 `Cargo.toml` 一致、位于 `main` 上，
   跑完测试后自动发布到 crates.io。
4. 消费方按自己的节奏升版本。

