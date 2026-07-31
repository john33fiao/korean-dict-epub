# ADR-0008: 전체 코퍼스 batch와 검증 상태

- 상태: 채택
- 날짜: 2026-07-31

## 배경

124권 전체 실행은 단권 명령 반복보다 실패·재개·병렬도와 검증 누락을 명시할
필요가 있습니다. 기존 결과를 암묵적으로 덮어쓰거나 EPUBCheck를 생략한
실행을 전체 통과로 기록하면 로컬 산출물의 상태를 신뢰하기 어렵습니다.

## 결정

- `batch`는 Git 추적 catalog를 파일 단위 worker queue로 처리합니다.
- 한 권의 단계는 EPUB 원자 생성, 독립 감사, 선택적 EPUBCheck 순서입니다.
  이전 단계가 실패하면 다음 단계는 실행하지 않습니다.
- 기본 실행은 선택 범위의 기존 EPUB이 하나라도 있으면 시작 전에
  `KDEP-E012`로 거부합니다.
- `--resume`은 기존 EPUB을 감사부터 다시 검사하고 누락 권만 생성합니다.
  `--overwrite`는 모든 권을 원자 재생성하며 두 옵션은 상호 배타적입니다.
- `--keep-going`이 없으면 첫 권 실패 후 새 할당을 중단합니다. 병렬 worker에
  이미 할당된 권은 완료될 수 있습니다.
- EPUBCheck는 `--epubcheck-jar`가 있을 때 `--failonwarnings --quiet`로
  실행합니다. 지정하지 않은 성공 batch는 `partial`로 기록합니다.
- `kdep-corpus-report-v1`은 expected/processed/passed/failed/unprocessed
  권 수, 총 항목 수와 권별 build/audit/EPUBCheck 상태·오류를 catalog 순서로
  기록합니다.
- setup 오류는 `KDEP-E012`, 권별 실패가 있는 완료 보고서는 `KDEP-E013`으로
  구분합니다.

## 이유

- 파일 단위 병렬화는 권 내부 순서를 바꾸지 않으면서 전체 시간을 줄입니다.
- 기본 no-clobber와 명시적 재개·재생성은 오래 걸린 로컬 결과를 보호합니다.
- EPUBCheck 생략을 `partial`로 분리하면 생성·감사 성공과 표준 검증 성공을
  혼동하지 않습니다.
- 실패한 권과 미처리 권을 따로 기록하면 `--keep-going` 정책과 실제 완료
  범위를 재구성할 수 있습니다.

## 결과

- Rust 전체 실행은 worker 2개와 `--resume --keep-going`, EPUBCheck 5.3.0
  조건에서 124/124권, 총 1,697,692항목을 처리했습니다.
- 독립 감사와 EPUBCheck는 124/124권 통과했고, EPUBCheck 경고·오류는
  없었습니다. XML 금지 제어문자 7개도 원본-EPUB 감사에서 일치했습니다.
- 생성 EPUB 124권의 로컬 합계는 약 925.7MiB이며 공개 저장소나 릴리스에
  포함하지 않습니다.
- Pandoc은 작은 합성 EPUB을 열었지만 실제 첫 권은 높은 메모리 사용으로
  완료하지 못했습니다. 일반 도구 재열기와 실제 앱·기기 수용은 자동 검증과
  별도 상태로 남습니다.
