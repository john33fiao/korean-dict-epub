# ADR-0004: 손실 없는 SourceRecord와 canonical digest v1

- 상태: 채택
- 날짜: 2026-07-31

## 배경

세 사전의 XML 구조는 서로 다르며 이후 upstream에 알려지지 않은 요소가
추가될 수 있습니다. 사전별 필드 모델이 데이터 포함 여부를 결정하면 새 필드,
속성 순서, 빈 요소와 mixed content의 tail 텍스트가 누락될 수 있습니다.
원본에는 XML 1.0 파서가 직접 수용하지 못하는 U+0008 byte도 있습니다.

## 결정

- `quick-xml` 0.41 계열의 pull reader로 XML을 스트리밍합니다.
- 보존 모델은 시작 요소, 빈 요소, 요소 텍스트, tail 텍스트와 종료 요소를
  구분하는 일반 `SourceRecord`입니다.
- 요소와 속성의 QName은 prefix를 포함한 입력 이름으로 기록하고, 속성은
  입력 순서를 유지합니다.
- 들여쓰기에만 쓰인 공백은 레코드에서 제외하지만, 공백을 포함한 의미 있는
  값은 앞뒤 공백까지 유지합니다.
- XML 1.0 금지 제어 byte는 파싱 전에 BMP PUA escape와 대응 codepoint로
  치환하고 레코드 생성 시 원래 제어문자로 복원합니다. 원본의 escape
  codepoint는 두 번 기록하여 표식과 구분합니다.
- canonical digest v1은 고정 preamble, variant tag, big-endian `u64` 길이와
  UTF-8 payload를 SHA-256에 순서대로 입력합니다.
- 주석, processing instruction, XML 선언과 DTD는 보존 레코드 범위에
  포함하지 않습니다. 이들이 텍스트를 나누더라도 인접한 논리 텍스트는
  합칩니다.

## 이유

- 이벤트 모델은 사전별 의미 사전과 무관하므로 미지 요소도 자동 보존됩니다.
- 시작·빈·종료 요소와 element/tail 텍스트의 구분이 순서 회귀를 드러냅니다.
- 길이 접두 binary token은 JSON 직렬화 구현에 의존하지 않고 다른 언어의
  독립 감사기에서도 재현할 수 있습니다.
- escape 문자 자체를 별도로 escape하면 실제 PUA 값과 제어문자 치환이
  충돌하지 않습니다.

## 결과

- digest는 논리 XML 레코드의 동일성을 검증하며 원본 XML byte-for-byte
  동일성을 의미하지 않습니다.
- XML declaration, 주석, processing instruction과 DTD 보존이 필요해지면
  digest schema version을 올리는 별도 결정을 먼저 해야 합니다.
- 사전별 항목 경계와 추적 XML catalog, 읽기 전용 검사 CLI는 KDEP-002의
  다음 구현 단위에서 이 레코드 스트림 위에 추가합니다.
