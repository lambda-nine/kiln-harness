cd /workspace || exit 1
sed 's/^port=8080$/port=9090/' app.conf > app.conf.new && mv app.conf.new app.conf
