cd /workspace || exit 2
[ -f sorted.csv ] || exit 1
cmp -s - sorted.csv <<'END' || exit 1
name,age,city
Hugo,5,Graz
Ben,9,Oslo
Eli,9,Kyoto
Iris,27,Dublin
Noor,27,Cairo
Ada,34,Nairobi
Mara,34,Lisbon
Cleo,41,Lima
Zoe,41,Quito
Dana,58,Austin
Omar,58,Tunis
Liam,100,Perth
END
[ "$(sha256sum people.csv | cut -d ' ' -f 1)" = "71928c33aeba98b4dba4baeca18b87f4398628ffda3edf15660a8b8ace8a9389" ] || exit 1
