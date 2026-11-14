# hash.sh: checksums of files.

# file_checksum FILE: the CRC and size cksum prints, without the name.
file_checksum() {
  cksum <"$1" | awk '{ print $1 " " $2 }'
}
