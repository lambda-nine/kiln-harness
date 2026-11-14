#!/bin/sh
# wordcount.sh FILE: words in FILE.
here=$(dirname "$0")
. "$here/../lib/core.sh"
. "$here/../lib/text/count.sh"
need_args 1 $# "wordcount.sh FILE"
count_words <"$1"
