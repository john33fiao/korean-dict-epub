# ADR-0011: Rust 1.95 MSRV

- 상태: 채택
- 날짜: 2026-07-31

## 배경

KDEP-001에서 Rust 2024 edition과 초기 dependency를 기준으로 MSRV 1.85를
선택했습니다. KWEB-003은 플랫폼별 system SQLite 차이를 제거하기 위해
`rusqlite 0.40.1`과 bundled `libsqlite3-sys 0.38.1`을 추가했습니다.

이 dependency는 `cfg_select!`를 사용합니다. Rust 1.94에서는 해당 표준
매크로가 아직 unstable이어서 dependency build script가 컴파일되지 않고,
Rust 1.95에서 안정화되어 같은 lockfile의 workspace check가 통과합니다.

## 결정

- Rust 2024 edition을 유지하고 프로젝트 MSRV를 Rust 1.95로 올립니다.
- `Cargo.toml`의 `rust-version`과 공개 요구사항·사용 문서를 1.95로 맞춥니다.
- MSRV 검증은 dependency resolver 성공만으로 판단하지 않고
  `cargo +1.95.0 test --workspace`와 clippy를 실제 실행해 확인합니다.
- 이 결정은 [`ADR-0003`](0003-rust-cli-foundation.md)의 MSRV 1.85 부분만
  대체하며 CLI·경로·오류 계약은 변경하지 않습니다.

## 이유

- `rusqlite 0.40.1`과 bundled SQLite를 유지하면서 실제로 컴파일되는 가장 낮은
  stable Rust 버전이 1.95입니다.
- dependency를 fork하거나 표준 매크로를 자체 backport하면 upstream 갱신과
  보안 수정 추적 부담이 생깁니다.
- manifest와 실제 compiler 요구사항을 일치시키면 지원되지 않는 toolchain이
  dependency build 중간에 실패하는 일을 사전에 명확히 알릴 수 있습니다.

## 결과

- Rust 1.94 이하 사용자는 build 전에 MSRV 불일치 안내를 받습니다.
- KWEB-003 schema·migration·DB 파일 형식은 바뀌지 않습니다.
- 향후 dependency 변경은 현재 MSRV에서 전체 workspace를 실제 검증한 뒤
  수용합니다.
