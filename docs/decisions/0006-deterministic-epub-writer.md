# ADR-0006: 결정적 단권 EPUB writer와 원자 교체

- 상태: 채택
- 날짜: 2026-07-31

## 배경

KDEP-002의 `SourceRecord`를 전자책 앱에서 순차적으로 읽을 XHTML로
직렬화하고 EPUB 3 package로 묶어야 합니다. 이 과정에서 의미 강조가
데이터 포함 여부를 바꾸거나, 실패한 실행이 부분 EPUB을 남기거나, 빌드 시각과
ZIP metadata 때문에 같은 입력의 결과가 매번 달라져서는 안 됩니다.

## 결정

- `render`는 모든 source record를 순서대로 독립 XHTML block으로 기록합니다.
  kind, 깊이, QName, 원래 순서의 속성, 값과 제어문자 codepoint를 audit 가능한
  markup으로 남깁니다.
- 표제어·뜻풀이·예문·번역 인식은 CSS class와 article heading만 추가하며
  generic record 생성에는 관여하지 않습니다.
- builder는 항목 하나만 메모리에 모으고, 완결된 300항목 또는 직렬화 약
  1MiB 중 먼저 도달한 경계에서 chapter를 나눕니다. 한 항목 내부는 나누지
  않습니다.
- OPF manifest/spine, nav와 ZIP member는 같은 chapter 목록에서 만듭니다.
- ZIP은 MSRV 1.85보다 낮은 MSRV 1.83을 지원하는 `zip` 7.0.0을 정확히
  고정해 사용합니다. `mimetype`은 첫 member이자 무압축이며 나머지는
  고정 level의 deflate를 사용합니다.
- ZIP timestamp와 OPF 수정 시각은 `1980-01-01T00:00:00Z`로 고정합니다.
  UUID v5 식별자는 고정 namespace와 사전 key·상대 XML 경로에서 계산합니다.
- 완성 ZIP은 `atomic-write-file` 0.3.0의 같은 디렉터리 temporary file에서
  member 목록과 mimetype을 점검한 뒤 commit합니다.
- 기존 출력은 기본 거부하며 `--overwrite`를 명시한 경우에만 원자적으로
  교체합니다. 출력 존재와 일반 build 실패는 각각 `KDEP-E008`,
  `KDEP-E009`로 구분합니다.

## 이유

- record markup과 semantic class를 분리하면 미지 필드도 동일하게 보존됩니다.
- 항목 하나만 버퍼링하면 전체 권 크기에 비례해 메모리가 증가하지 않으면서
  article과 장 경계를 안정적으로 만들 수 있습니다.
- chapter 목록을 단일 기준으로 사용하면 OPF, nav와 ZIP member 불일치를
  줄일 수 있습니다.
- 고정 metadata와 atomic commit은 재현 가능한 결과와 실패 시 기존 파일
  보존을 함께 제공합니다.

## 결과

- generic record markup은 원본 XML보다 장황하므로 byte 제한이 300항목보다
  먼저 적용될 수 있습니다.
- `build`는 현재 한 권만 처리합니다. 전체 124권 orchestration과 보고서는
  후속 티켓에서 추가합니다.
- 내부 package 점검을 구현하고 대표 단권 EPUBCheck를 실행했지만,
  원본-EPUB 독립 record 감사는 KDEP-004에서 별도 코드 경로로 구현합니다.
