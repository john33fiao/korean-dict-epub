# References

이 디렉터리는 변환기가 읽는 외부 원본 저장소를 둡니다.

## `korean-dict-nikl`

- 원격: `https://github.com/spellcheck-ko/korean-dict-nikl.git`
- 형태: Git 서브모듈
- 역할: 한국어기초사전, 표준국어대사전, 우리말샘 XML 입력

부모 저장소에서는 서브모듈의 고정 커밋만 관리합니다. 원본 XML, upstream
갱신 스크립트, 라이선스 안내를 이 저장소로 복제하거나 수정하지 않습니다.
서브모듈 내부에서 생성한 EPUB이나 로컬 미추적 파일도 부모 저장소에 포함하지
않습니다.

초기화:

```powershell
git submodule update --init
```
