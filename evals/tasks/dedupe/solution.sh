cd /workspace || exit 1
awk '!seen[$0]++' list.txt > list.txt.new && mv list.txt.new list.txt
