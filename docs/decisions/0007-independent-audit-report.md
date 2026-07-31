# ADR-0007: 독립 원본-EPUB 감사와 결정적 보고서

- 상태: 채택
- 날짜: 2026-07-31

## 배경

변환기가 계산한 digest나 manifest를 그대로 신뢰하면 source reader,
renderer와 검증기가 같은 결함을 공유할 수 있습니다. 보존 주장은 완성 EPUB의
화면 텍스트나 package 유효성만으로도 충분하지 않으며, 원본의 레코드 순서와
속성·제어문자를 다시 복원해 대조해야 합니다.

## 결정

- `audit`은 catalog로 선택한 원본 XML과 표준 파일명의 완성 EPUB을 직접
  읽습니다.
- 원본 감사 경로는 `source`·`record` 모듈을 호출하지 않습니다. 별도
  control sanitizer, streaming frame과 canonical digest v1 encoder를
  유지합니다.
- 원본 sanitizer는 변환기의 BMP PUA prefix와 다른 prefix를 사용하고 실제
  prefix 문자를 중복 escape하여 제어문자 표식과 충돌하지 않게 합니다.
- EPUB 감사 경로는 `EPUB/package.opf`의 manifest와 spine을 직접 읽고 XHTML
  record markup에서 kind, 깊이, QName, 원래 순서의 속성, text/tail과
  `data-codepoint` 제어문자를 재구성합니다.
- title page에 기록된 항목 수와 digest는 감사 입력으로 신뢰하지 않습니다.
- record digest·개수, 항목 수, 표제어 순서 digest, 첫/마지막 표제어와
  identifier·title·language·source·modified·collection·권 metadata를
  각각 대조합니다.
- 결과는 `kdep-audit-report-v1` JSON으로 EPUB 옆에 원자 교체합니다.
  내용 불일치는 `KDEP-E011`, 읽기·구조·직렬화 실패는 `KDEP-E010`입니다.

## 이유

- 별도 구현은 변환기의 누락이나 순서 결함이 자체 검증도 함께 통과할 위험을
  낮춥니다.
- OPF spine부터 다시 읽으면 archive에 존재하기만 하고 독서 순서에서 빠진
  XHTML을 보존 증거로 잘못 세지 않습니다.
- check별 expected/actual 값은 전체 코퍼스 실패를 단권 명령으로 재현할 수
  있게 합니다.
- 시각용 제어문자 glyph 대신 `data-codepoint`를 역변환하면 원래 codepoint
  손상을 검출할 수 있습니다.

## 결과

- 감사 코드에는 canonical digest schema와 XML event 처리의 의도적 중복이
  생깁니다. schema 변경 시 두 경로와 negative test를 함께 갱신해야 합니다.
- 동일 XML library를 사용하지만 sanitizer, frame, digest와 출력 복원 상태는
  공유하지 않습니다. Python 기준선 비교를 추가 독립 근거로 유지합니다.
- 세 사전 첫 권의 항목 수와 첫/마지막 표제어는 Python 기준선과 일치했고,
  Rust 원본-EPUB record digest도 각각 일치했습니다.
- 전체 124권 배치 감사, 통합 보고서, EPUBCheck와 실제 리더 QA는 KDEP-005
  범위입니다.
