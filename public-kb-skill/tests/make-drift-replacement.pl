#!/usr/bin/env perl
use strict;
use warnings;

my $mode = shift @ARGV // die "usage: make-drift-replacement.pl MODE\n";

if ($mode eq 'command') {
  print "\n    (\"future\", None, &[], &[], false),\n];\n";
  exit 0;
}

if ($mode eq 'alias') {
  print "\n    \"bk\" => \"backup\",\n    other => other,\n  }\n}\n";
  exit 0;
}

die "unknown mode: $mode\n";
