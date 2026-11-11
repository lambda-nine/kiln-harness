cd /workspace || exit 2
[ -f docs/install.md ] || exit 1
[ ! -e docs/install.txt ] || exit 1
[ "$(sha256sum docs/install.md | cut -d ' ' -f 1)" = "87fa741eb9ab492358e081dd06df7fa08c3de007d7a890cf57e487ad50a2aafa" ] || exit 1
[ -f docs/usage.md ] || exit 1
[ ! -e docs/usage.txt ] || exit 1
[ "$(sha256sum docs/usage.md | cut -d ' ' -f 1)" = "0f5e4c6260b617b0d9cbc97d6cbce8b8e8604af2d62fa774fd809022051432f2" ] || exit 1
[ -f docs/changes.md ] || exit 1
[ ! -e docs/changes.txt ] || exit 1
[ "$(sha256sum docs/changes.md | cut -d ' ' -f 1)" = "fedd8fbaf0d202c2b77402d1e2c5fc70d41453f8cda8d925aa9a666129dae85d" ] || exit 1
[ -f docs/faq.md ] || exit 1
[ ! -e docs/faq.txt ] || exit 1
[ "$(sha256sum docs/faq.md | cut -d ' ' -f 1)" = "dcae792828bf231ec9c809c1a5540e15b86f04e8a4de18da3b89e9a3b066a74d" ] || exit 1
[ -z "$(find docs -name '*.txt')" ] || exit 1
