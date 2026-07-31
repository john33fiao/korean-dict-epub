# ADR-0010: SQLite corpus schema와 migration v1

- 상태: 채택
- 날짜: 2026-07-31

## 배경

세 사전 XML을 웹 조회용 DB로 적재하되 원문 보존, 안정 식별자, 관계 판정과
조회 projection을 서로 구분해야 합니다. 장비마다 다른 system SQLite나
암묵적인 `rowid`·type coercion에 기대면 같은 입력의 저장 결과와 검증 계약이
달라질 수 있습니다.

## 결정

- `rusqlite 0.40.1`의 bundled SQLite를 사용합니다. schema v1의 application
  ID는 `0x4B574542`, `PRAGMA user_version`은 `1`입니다. 이 dependency의
  최소 compiler는 [`ADR-0011`](0011-rust-1-95-msrv.md)에 따라 Rust 1.95로
  고정합니다.
- application table은 모두 `STRICT, WITHOUT ROWID`로 만들고 명시적
  primary key, foreign key, unique와 check constraint를 사용합니다. 원본
  native key는 선행 0을 보존하는 `TEXT`이며 canonical ID와 분리합니다.
- `corpus`·`source_file`은 source commit과 파일 순서를, `entity`는
  `kweb:v1/...` canonical ID, 사전·kind·부모 entry·source locator와 원본
  순서를 저장합니다.
- `source_record`·`source_attribute`는 QName, 속성 순서, 빈 요소, element
  text와 tail을 손실 없이 저장합니다. `entry_projection`·`text_projection`은
  표제어·동형어·품사·뜻풀이·예문·번역 같은 조회값을 원본 위치·순서와 함께
  별도로 저장합니다.
- `relation`·`relation_raw_field`·`relation_candidate`는 원문 필드, 후보,
  판정 이유와 `resolved`·`self_reference`·`unresolved`·`ambiguous` 상태를
  보존합니다.
- migration은 정렬된 내장 SQL과 SHA-256 checksum을 사용합니다. 각 migration은
  `BEGIN IMMEDIATE` transaction에서 schema 변경, history 기록,
  `application_id`·`user_version` 갱신을 함께 commit하며 forward migration만
  제공합니다.
- application ID가 0인 완전한 빈 DB만 초기화합니다. 다른 application ID,
  checksum 불일치와 지원 버전보다 새로운 DB는 수정하지 않고 거부합니다.
- 모든 연결은 `foreign_keys=ON`, `trusted_schema=OFF`와 명시적 busy timeout을
  사용합니다. 사용자 선택 DB 검증은 URI 해석을 켜지 않은 read-only 연결로
  일반 파일만 열고, `quick_check`·`foreign_key_check`, marker, schema
  fingerprint와 migration history를 확인합니다.
- `ReadyCorpus` 검증은 정확히 하나인 ready corpus, source commit·counts·파일
  metadata, canonical namespace·entity owner와 대표 entry projection 조회까지
  추가로 확인합니다. 검증 성공 descriptor만 이후 active 설정 API의 입력이 될
  수 있습니다.
- JSON·FTS·ANN과 암묵적 row order는 schema v1에 사용하지 않습니다. 향후
  재생성 가능한 table은 `derived_*` namespace로 원본·projection table과
  구분합니다.

## 이유

- bundled SQLite와 strict typing은 플랫폼별 engine 차이와 조용한 type 변환을
  줄입니다.
- 원문 레코드와 조회 projection을 분리하면 새 projection을 추가하거나
  재생성해도 데이터 포함 여부가 달라지지 않습니다.
- migration SQL checksum과 전체 schema fingerprint를 함께 확인하면 version
  숫자만 같은 변형 DB를 활성화하는 위험을 줄입니다.
- read-only 검증과 성공 descriptor 경계는 잘못된 사용자 선택이 기존 active
  DB를 대체하거나 선택 파일을 조용히 변경하는 일을 막습니다.

## 결과

- KWEB-004 importer는 이 schema와 공용 canonical ID API를 사용하고, ready
  전환 전 source digest·파일 수·entry 수를 채워야 합니다.
- 전체 124권 적재, 독립 DB 감사, 검색 projection·FTS/vector와 PostgreSQL
  구현은 각각 후속 티켓에서 별도로 검증합니다.
- 생성 SQLite DB와 journal·WAL·shared-memory 파일은 Git 추적 대상이 아닙니다.
