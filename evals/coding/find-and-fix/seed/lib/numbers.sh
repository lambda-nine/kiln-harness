# numbers.sh: integer helpers.

# clamp VALUE LOW HIGH: VALUE kept within LOW and HIGH.
clamp() {
  if [ "$1" -lt "$2" ]; then
    echo "$2"
  elif [ "$1" -gt "$3" ]; then
    echo "$3"
  else
    echo "$1"
  fi
}

# max_of NUMBER...: the largest.
max_of() {
  _mx=$1
  for _n in "$@"; do
    [ "$_n" -gt "$_mx" ] && _mx=$_n
  done
  echo "$_mx"
}

# min_of NUMBER...: the smallest.
min_of() {
  _mn=$1
  for _n in "$@"; do
    [ "$_n" -lt "$_mn" ] && _mn=$_n
  done
  echo "$_mn"
}

# sum_list NUMBER...: their sum.
sum_list() {
  _sum=0
  for _n in "$@"; do
    _sum=$((_sum + _n))
  done
  echo "$_sum"
}

# is_even NUMBER
is_even() {
  [ $(( $1 % 2 )) -eq 0 ]
}
