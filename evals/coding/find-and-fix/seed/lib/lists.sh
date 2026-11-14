# lists.sh: helpers for argument lists.

# join_by SEPARATOR ITEM...: the items joined by SEPARATOR.
join_by() {
  _sep=$1
  shift
  _out=''
  _first=1
  for _item in "$@"; do
    if [ "$_first" -eq 1 ]; then
      _out=$_item
      _first=0
    else
      _out=$_out$_sep$_item
    fi
  done
  printf '%s\n' "$_out"
}

# count_items ITEM...: how many items.
count_items() {
  echo "$#"
}

# first_item ITEM...
first_item() {
  printf '%s\n' "$1"
}

# last_item ITEM...
last_item() {
  for _item in "$@"; do :; done
  printf '%s\n' "$_item"
}

# contains_item NEEDLE ITEM...
contains_item() {
  _needle=$1
  shift
  for _item in "$@"; do
    [ "$_item" = "$_needle" ] && return 0
  done
  return 1
}
