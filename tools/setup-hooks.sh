#!/usr/bin/env bash
# 安装本仓 git 钩子（仓库卫生闸等）：git config core.hooksPath .githooks
# 新克隆后跑一次即可；CI 侧门禁（hygiene job）不依赖此步骤。
set -e
cd "$(dirname "$0")/.."
git config core.hooksPath .githooks
echo "✓ hooks 已安装（core.hooksPath=.githooks）：pre-commit 将对 staged 文件跑仓库卫生检查"
