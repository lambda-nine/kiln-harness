# duration.sh: lengths of time for people.

# human_duration SECONDS: such as 1h 2m 3s, leaving out leading zero units.
human_duration() {
  _hd_h=$(( $1 / 3600 ))
  _hd_m=$(( $1 % 3600 / 60 ))
  _hd_s=$(( $1 % 60 ))
  if [ "$_hd_h" -gt 0 ]; then
    echo "${_hd_h}h ${_hd_m}m ${_hd_s}s"
  elif [ "$_hd_m" -gt 0 ]; then
    echo "${_hd_m}m ${_hd_s}s"
  else
    echo "${_hd_s}s"
  fi
}
