"src/" {
    sub(/.*"src\//, "");
    sub(/".*/, "");
    if ($0 !~ /\//) print "src/" $0;
}
