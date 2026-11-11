#!/bin/sh
# Print the sum of the integers from 1 to N, where N is the first argument.
n=$1
total=0
i=1
while [ "$i" -lt "$n" ]; do
  total=$((total + i))
  i=$((i + 1))
done
echo "$total"
