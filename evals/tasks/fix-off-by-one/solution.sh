cd /workspace || exit 1
sed 's/-lt "\$n"/-le "$n"/' sum.sh > sum.sh.new && mv sum.sh.new sum.sh
