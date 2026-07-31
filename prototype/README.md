# Python Prototype

이 디렉터리는 2026-07-30 전체 124권 생성과 검증에 사용한 Python 기준선을
보존합니다. 앞으로의 주 구현은 Rust이며, 이 코드는 Rust 구현의 보존 결과와
회귀를 비교하는 용도입니다.

## 구성

- `python/build_epubs.py`: 전체 또는 선택한 XML의 EPUB 생성과 내부 검증
- `python/audit_epubs.py`: 원본 XML과 완성 EPUB의 독립 필드 감사
- `python/run_epubcheck.py`: manifest에 포함된 EPUB 전권 EPUBCheck 실행

세 스크립트는 Python 표준 라이브러리만 사용합니다. EPUBCheck 실행에는 별도의
Java와 EPUBCheck JAR가 필요합니다.

## 생성

```powershell
python prototype/python/build_epubs.py build `
  --source references/korean-dict-nikl `
  --output outputs/python-baseline `
  --jobs 3
```

기존 출력을 덮어쓰려면 `--overwrite`, 한 권이 실패해도 나머지를 계속하려면
`--keep-going`을 명시해야 합니다. 특정 사전이나 파일만 실행할 때는
`--only`의 실제 허용값을 `--help`와 코드에서 먼저 확인하십시오.

## 내부 검증

```powershell
python prototype/python/build_epubs.py validate `
  --output outputs/python-baseline `
  --jobs 3
```

## 독립 원본 감사

```powershell
python prototype/python/audit_epubs.py `
  --output outputs/python-baseline `
  --jobs 3
```

감사기는 출력 manifest가 가리키는 원본과 EPUB을 다시 읽습니다. 서브모듈
커밋이나 원본 경로가 달라졌다면 이전 보고서를 현재 입력의 증거로 사용하지
마십시오.

## EPUBCheck

```powershell
python prototype/python/run_epubcheck.py `
  --output outputs/python-baseline `
  --jar C:\path\to\epubcheck.jar `
  --jobs 3
```

전권 변환과 검사는 오래 걸리고 많은 디스크를 사용합니다. 처음에는
`--only`로 대표 XML 한 권을 생성해 화면과 보고서를 확인하는 편이 안전합니다.
