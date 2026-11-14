cd /workspace || exit 1
patch -p1 <<'PATCH_END'
--- seed/invoice.awk
+++ good/invoice.awk
@@ -245,6 +245,7 @@
     if (quoted) {
       if (c == "\"") {
         if (substr(line, i + 1, 1) == "\"") {
+          field = field "\""
           i++
         } else {
           quoted = 0
@@ -771,6 +772,7 @@
 # round_div(n, d): n / d rounded to the nearest whole number, halves away
 # from zero, for a positive d: 5/2 is 3 and -5/2 is -3.
 function round_div(n, d,   q, r) {
+  if (n < 0) return -round_div(-n, d)
   q = int(n / d)
   r = n - q * d
   if (r * 2 >= d) q++
@@ -1290,6 +1292,7 @@
   for (i = 1; i <= len; i++) {
     c = substr(s, i, 1)
     if (c == "\"") out = out "\\\""
+    else if (c == "\\") out = out "\\\\"
     else if (c == "\t") out = out "\\t"
     else if (c == "\r") out = out "\\r"
     else if (c == "\n") out = out "\\n"
PATCH_END
