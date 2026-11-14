# clock.sh: times of day as seconds since midnight.

# to_seconds HOURS MINUTES SECONDS
to_seconds() {
  echo $(( $1 * 3600 + $2 * 60 + $3 ))
}

# hms SECONDS: H:MM:SS.
hms() {
  printf '%d:%02d:%02d\n' $(( $1 / 3600 )) $(( $1 % 3600 / 60 )) $(( $1 % 60 ))
}
