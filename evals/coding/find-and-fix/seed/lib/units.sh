# units.sh: unit conversions, rounded toward zero.

# c_to_f CELSIUS
c_to_f() {
  echo $(( $1 * 9 / 5 + 32 ))
}

# f_to_c FAHRENHEIT
f_to_c() {
  echo $(( ($1 - 32) * 5 / 9 ))
}

# km_to_m KILOMETRES
km_to_m() {
  echo $(( $1 * 1000 ))
}
