# 000 リポの初期整備

## 目的
public の OSS として、最初の commit から衛生と CI がそろった状態にする。

## 作業
- 当該リポだけ commit author を GitHub の noreply に固定する（`--global` にしない）
- 既定ブランチを dbboard と同じ運用にそろえる（develop で作業し、main はタグ付きの版だけ）
- Rust の workspace を置き、CI で `cargo fmt --check` / `cargo clippy -- -D warnings` / `cargo test` を回す
- `escalation` / `objection` ラベルを public 用の文言で作る
- description と topics を置く（README の 1 行目と同じ語で）

## 完了条件
- [ ] 既定ブランチの最新 run が緑（`gh run list --repo meta-taro/umbrachyx --limit 3`）
- [ ] `git log --format='%ae' | sort -u` が noreply だけ
- [ ] `gh label list --repo meta-taro/umbrachyx` に escalation / objection がある
