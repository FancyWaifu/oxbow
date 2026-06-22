# A real awk program, run by onetrueawk on oxbow via the musl personality.
{ n++; total += $1 }
END {
    printf "lines=%d  sum=%d  avg=%.2f  max=%s\n", n, total, total/n, max
}
$1 > max { max = $1 }
