#!/usr/bin/env perl
use strict;
use warnings;

sub usage {
  die "usage: make-spoofed-surface.pl MODE FIXTURE_FILE SOURCE_FILE [duplicate]\n";
}

my $mode = shift @ARGV // usage();
my $fixture_file = shift @ARGV // usage();
my $source_file = shift @ARGV // usage();
my $duplicate = shift @ARGV // '';

open my $in, '<', $fixture_file or die "open($fixture_file): $!";
my @rows = grep { length($_) } map { chomp; $_ } <$in>;
close $in;

my $dir = $source_file;
$dir =~ s{/[^/]+\z}{};
system('mkdir', '-p', $dir) == 0 or die "mkdir($dir): $!";
open my $out, '>', $source_file or die "open($source_file): $!";

sub print_prelude {
  my ($out) = @_;
  print {$out} <<'EOF';
const SPOOFED_TEXT: &str = "pub(crate) const COMMANDS: &[CommandRow] = &[
    (\"phantom\", None, &[], &[], false),
];
fn canonical_command(value: &str) -> &str {
    match value {
        \"ghost\" => \"shadow\",
        other => other,
    }
}
/* outer comment start
   /* nested comment */
   comment end */
escaped quote: \\\" backslash: \\\\ and char: '\''
";
const SPOOFED_RAW: &str = r###"
pub(crate) const COMMANDS: &[CommandRow] = &[
    ("phantom_raw", None, &[], &[], false),
];
fn canonical_command(value: &str) -> &str {
    match value {
        "ghost_raw" => "shadow_raw",
        other => other,
    }
}
/* raw comment markers /* ignored */ */
"###;
const SPOOFED_RAW_BYTE: &str = br##"
pub(crate) const COMMANDS: &[CommandRow] = &[
    ("phantom_br", None, &[], &[], false),
];
fn canonical_command(value: &str) -> &str {
    match value {
        "ghost_br" => "shadow_br",
        other => other,
    }
}
/* raw byte markers /* ignored */ */
"##;
EOF
}

sub print_commands_real {
  my ($out, $rows) = @_;
  print {$out} "pub(crate) const COMMANDS: &[CommandRow] = &[\n";
  for my $row (@$rows) {
    next if $row eq '';
    print {$out} qq{    ("$row", None, &[], &[], false),\n};
  }
  print {$out} "];\n";
}

sub print_aliases_real {
  my ($out, $rows) = @_;
  print {$out} "fn canonical_command(value: &str) -> &str {\n";
  print {$out} "  match value {\n";
  for my $row (@$rows) {
    next if $row eq '';
    my ($alias, $canonical) = split /\t/, $row, 2;
    print {$out} qq{    "$alias" => "$canonical",\n};
  }
  print {$out} "    other => other,\n";
  print {$out} "  }\n";
  print {$out} "}\n";
}

print_prelude($out);

if ($mode eq 'commands') {
  print_commands_real($out, \@rows);
  print_commands_real($out, \@rows) if $duplicate eq 'duplicate';
  exit 0;
}

if ($mode eq 'aliases') {
  print_aliases_real($out, \@rows);
  print_aliases_real($out, \@rows) if $duplicate eq 'duplicate';
  exit 0;
}

die "unknown mode: $mode\n";
