#!/usr/bin/env perl
use strict;
use warnings;

sub fail {
  die "$_[0]\n";
}

sub is_ident_start {
  return $_[0] =~ /[A-Za-z_]/;
}

sub is_ident_continue {
  return $_[0] =~ /[A-Za-z0-9_]/;
}

sub try_parse_raw_string {
  my ($s, $i_ref) = @_;
  my $start = $$i_ref;
  my $len = length($s);
  my $prefix_len;

  if ($start + 1 < $len && substr($s, $start, 2) =~ /^(?:br|rb)$/) {
    $prefix_len = 2;
  } elsif (substr($s, $start, 1) eq 'r') {
    $prefix_len = 1;
  } else {
    return;
  }

  my $i = $start + $prefix_len;
  my $hashes = 0;
  while ($i < $len && substr($s, $i, 1) eq '#') {
    $hashes++;
    $i++;
  }
  return if $i >= $len || substr($s, $i, 1) ne '"';

  my $content_start = $i + 1;
  $i++;
  while ($i < $len) {
    if (substr($s, $i, 1) eq '"') {
      my $matched = 1;
      for my $k (1 .. $hashes) {
        if ($i + $k >= $len || substr($s, $i + $k, 1) ne '#') {
          $matched = 0;
          last;
        }
      }
      if ($matched) {
        my $value = substr($s, $content_start, $i - $content_start);
        $$i_ref = $i + 1 + $hashes;
        return { kind => 'rawstring', value => $value };
      }
    }
    $i++;
  }

  fail "unterminated raw string";
}

sub try_parse_char_literal {
  my ($s, $i_ref) = @_;
  my $start = $$i_ref;
  my $len = length($s);

  return unless substr($s, $start, 1) eq chr(39);

  my $i = $start + 1;
  return if $i >= $len;

  my $first = substr($s, $i, 1);
  return if $first eq "\n" || $first eq "\r";

  if ($first eq '\\') {
    $i += 2;
  } elsif ($i + 1 < $len && substr($s, $i + 1, 1) eq chr(39)) {
    $i += 2;
  } else {
    return;
  }

  while ($i < $len) {
    my $ch = substr($s, $i, 1);
    if ($ch eq chr(39)) {
      my $value = substr($s, $start + 1, $i - $start - 1);
      $$i_ref = $i + 1;
      return { kind => 'char', value => $value };
    }
    return if $ch eq "\n" || $ch eq "\r";
    $i++;
  }

  fail "unterminated char literal";
}

sub tokenize_rust {
  my ($s) = @_;
  my @tokens;
  my $len = length($s);
  my $i = 0;

  while ($i < $len) {
    my $c = substr($s, $i, 1);
    my $n = $i + 1 < $len ? substr($s, $i + 1, 1) : '';

    if ($c =~ /\s/) {
      $i++;
      next;
    }

    if ($c eq '/' && $n eq '/') {
      $i += 2;
      while ($i < $len && substr($s, $i, 1) ne "\n") {
        $i++;
      }
      next;
    }

    if ($c eq '/' && $n eq '*') {
      $i += 2;
      my $depth = 1;
      while ($i < $len && $depth > 0) {
        my $ch = substr($s, $i, 1);
        my $nx = $i + 1 < $len ? substr($s, $i + 1, 1) : '';
        if ($ch eq '/' && $nx eq '*') {
          $depth++;
          $i += 2;
          next;
        }
        if ($ch eq '*' && $nx eq '/') {
          $depth--;
          $i += 2;
          next;
        }
        $i++;
      }
      fail "unterminated block comment" if $depth > 0;
      next;
    }

    if (my $raw = try_parse_raw_string($s, \$i)) {
      push @tokens, $raw;
      next;
    }

    if ($c eq '"') {
      my $start = $i++;
      my $closed = 0;
      while ($i < $len) {
        my $ch = substr($s, $i, 1);
        if ($ch eq '\\') {
          $i += 2;
          next;
        }
        if ($ch eq '"') {
          my $value = substr($s, $start + 1, $i - $start - 1);
          $i++;
          push @tokens, { kind => 'string', value => $value };
          $closed = 1;
          last;
        }
        $i++;
      }
      next if $closed;
      fail "unterminated string literal";
    }

    if (my $char = try_parse_char_literal($s, \$i)) {
      push @tokens, { kind => 'char', value => $char->{value} };
      next;
    }

    if (is_ident_start($c)) {
      my $start = $i++;
      while ($i < $len && is_ident_continue(substr($s, $i, 1))) {
        $i++;
      }
      push @tokens, { kind => 'ident', value => substr($s, $start, $i - $start) };
      next;
    }

    my $matched_punct = 0;
    for my $pair (qw(:: -> => == != <= >= && || ..)) {
      if (substr($s, $i, 2) eq $pair) {
        push @tokens, { kind => 'punct', value => $pair };
        $i += 2;
        $matched_punct = 1;
        last;
      }
    }
    if ($matched_punct) {
      next;
    }

    push @tokens, { kind => 'punct', value => $c };
    $i++;
  }

  return \@tokens;
}

sub token_is {
  my ($token, $kind, $value) = @_;
  return 0 if $token->{kind} ne $kind;
  return 1 if !defined $value;
  return $token->{value} eq $value;
}

sub literal_value {
  my ($token) = @_;
  return $token->{value} if $token->{kind} eq 'string' || $token->{kind} eq 'rawstring' || $token->{kind} eq 'char';
  fail "expected literal token";
}

sub token_text {
  my ($token) = @_;
  return $token->{value};
}

sub match_sequence_at {
  my ($tokens, $index, $sequence) = @_;
  for my $offset (0 .. $#$sequence) {
    return 0 if $index + $offset > $#$tokens;
    return 0 if token_text($tokens->[$index + $offset]) ne $sequence->[$offset];
  }
  return 1;
}

sub top_level_match_starts {
  my ($tokens, $sequence) = @_;
  my @matches;

  for (my $i = 0; $i < @$tokens; $i++) {
    push @matches, $i if match_sequence_at($tokens, $i, $sequence);
  }

  return @matches;
}

sub skip_to_token {
  my ($tokens, $index, $wanted, $limit) = @_;
  my $max_index = defined $limit ? $limit : $#$tokens;
  while ($index <= $max_index && $index < @$tokens) {
    return $index if token_is($tokens->[$index], 'punct', $wanted);
    $index++;
  }
  return;
}

sub extract_command_rows {
  my ($tokens) = @_;
  my @starts = top_level_match_starts($tokens, [qw(pub ( crate ) const COMMANDS)]);
  fail "missing COMMANDS table" unless @starts;
  fail "duplicate COMMANDS declaration" if @starts > 1;

  my $index = $starts[0];
  $index = skip_to_token($tokens, $index, '=', $index + 12) // fail "COMMANDS declaration is malformed";
  $index++;
  $index = skip_to_token($tokens, $index, '[', $index + 4) // fail "COMMANDS declaration is malformed";

  my $body_start = $index + 1;
  my $depth = 1;
  my $body_end;
  for (my $i = $body_start; $i < @$tokens; $i++) {
    my $token = $tokens->[$i];
    next unless $token->{kind} eq 'punct';
    $depth++ if $token->{value} eq '[';
    if ($token->{value} eq ']') {
      $depth--;
      if ($depth == 0) {
        $body_end = $i;
        last;
      }
    }
    fail "COMMANDS declaration is malformed" if $depth < 0;
  }

  defined $body_end or fail "COMMANDS declaration is malformed";
  my $after = $body_end + 1;
  fail "COMMANDS declaration is malformed" if $after >= @$tokens || !token_is($tokens->[$after], 'punct', ';');

  my @commands;
  my $i = $body_start;
  while ($i < $body_end) {
    my $token = $tokens->[$i];
    if (token_is($token, 'punct', ',')) {
      $i++;
      next;
    }

    fail "COMMANDS declaration is malformed" unless token_is($token, 'punct', '(');
    $i++;
    fail "COMMANDS declaration is malformed" if $i >= $body_end;
    my $name = $tokens->[$i];
    fail "COMMANDS declaration is malformed" unless $name->{kind} eq 'string' || $name->{kind} eq 'rawstring';
    push @commands, literal_value($name);
    $i++;

    my ($paren_depth, $bracket_depth, $brace_depth) = (1, 0, 0);
    while ($i < $body_end) {
      my $current = $tokens->[$i];
      if ($current->{kind} eq 'punct') {
        $paren_depth++ if $current->{value} eq '(';
        $paren_depth-- if $current->{value} eq ')';
        $bracket_depth++ if $current->{value} eq '[';
        $bracket_depth-- if $current->{value} eq ']';
        $brace_depth++ if $current->{value} eq '{';
        $brace_depth-- if $current->{value} eq '}';
        fail "COMMANDS declaration is malformed" if $paren_depth < 0 || $bracket_depth < 0 || $brace_depth < 0;
      }
      $i++;
      last if $paren_depth == 0 && $bracket_depth == 0 && $brace_depth == 0;
    }

    fail "COMMANDS declaration is malformed" if $paren_depth != 0 || $bracket_depth != 0 || $brace_depth != 0;
  }

  fail "COMMANDS declaration is malformed" unless @commands;
  return @commands;
}

sub top_level_punct_sequence {
  my ($tokens, $start, $wanted) = @_;
  my @hits;
  my $brace_depth = 0;

  for (my $i = $start; $i < @$tokens; $i++) {
    if ($brace_depth == 0 && match_sequence_at($tokens, $i, $wanted)) {
      push @hits, $i;
    }
    my $token = $tokens->[$i];
    if ($token->{kind} eq 'punct') {
      $brace_depth++ if $token->{value} eq '{';
      $brace_depth-- if $token->{value} eq '}';
    }
  }

  return @hits;
}

sub extract_function_body {
  my ($tokens) = @_;
  my @starts = top_level_match_starts($tokens, [qw(fn canonical_command)]);
  fail "missing canonical_command function" unless @starts;
  fail "duplicate canonical_command function" if @starts > 1;

  my $index = $starts[0];
  $index++;
  $index = skip_to_token($tokens, $index, '{', $index + 10) // fail "canonical_command function is malformed";

  my $body_start = $index + 1;
  my $depth = 1;
  my $body_end;
  for (my $i = $body_start; $i < @$tokens; $i++) {
    my $token = $tokens->[$i];
    next unless $token->{kind} eq 'punct';
    $depth++ if $token->{value} eq '{';
    if ($token->{value} eq '}') {
      $depth--;
      if ($depth == 0) {
        $body_end = $i;
        last;
      }
    }
    fail "canonical_command function is malformed" if $depth < 0;
  }

  defined $body_end or fail "canonical_command function is malformed";
  return ($body_start, $body_end);
}

sub extract_alias_pairs {
  my ($tokens, $body_start, $body_end) = @_;

  my @match_starts = top_level_punct_sequence($tokens, $body_start, [qw(match value)]);
  fail "canonical_command function is malformed" unless @match_starts;
  fail "duplicate canonical_command match" if @match_starts > 1;

  my $index = $match_starts[0];
  $index += 2;
  $index = skip_to_token($tokens, $index, '{', $index + 10) // fail "canonical_command function is malformed";

  my $match_start = $index + 1;
  my $depth = 1;
  my $match_end;
  for (my $i = $match_start; $i < $body_end; $i++) {
    my $token = $tokens->[$i];
    next unless $token->{kind} eq 'punct';
    $depth++ if $token->{value} eq '{';
    if ($token->{value} eq '}') {
      $depth--;
      if ($depth == 0) {
        $match_end = $i;
        last;
      }
    }
    fail "canonical_command function is malformed" if $depth < 0;
  }

  defined $match_end or fail "canonical_command function is malformed";

  my @pairs;
  my $passthrough_count = 0;
  my @arm;
  my ($paren_depth, $bracket_depth, $brace_depth) = (0, 0, 0);
  for (my $i = $match_start; $i < $match_end; $i++) {
    my $token = $tokens->[$i];
    if ($token->{kind} eq 'punct') {
      if ($token->{value} eq ',' && $paren_depth == 0 && $bracket_depth == 0 && $brace_depth == 0) {
        my $result = process_alias_arm(\@arm);
        if ($result->{passthrough}) {
          $passthrough_count++;
          fail "duplicate canonical_command passthrough" if $passthrough_count > 1;
        } else {
          push @pairs, @{ $result->{pairs} };
        }
        @arm = ();
        next;
      }
      $paren_depth++ if $token->{value} eq '(';
      $paren_depth-- if $token->{value} eq ')';
      $bracket_depth++ if $token->{value} eq '[';
      $bracket_depth-- if $token->{value} eq ']';
      $brace_depth++ if $token->{value} eq '{';
      $brace_depth-- if $token->{value} eq '}';
      fail "canonical_command function is malformed" if $paren_depth < 0 || $bracket_depth < 0 || $brace_depth < 0;
    }
    push @arm, $token;
  }
  if (@arm) {
    my $result = process_alias_arm(\@arm);
    if ($result->{passthrough}) {
      $passthrough_count++;
      fail "duplicate canonical_command passthrough" if $passthrough_count > 1;
    } else {
      push @pairs, @{ $result->{pairs} };
    }
  }

  return @pairs;
}

sub process_alias_arm {
  my ($arm) = @_;
  my ($arrow_index, $paren_depth, $bracket_depth, $brace_depth) = (-1, 0, 0, 0);

  for (my $i = 0; $i < @$arm; $i++) {
    my $token = $arm->[$i];
    if ($token->{kind} eq 'punct') {
      if ($token->{value} eq '=>'
        && $paren_depth == 0 && $bracket_depth == 0 && $brace_depth == 0) {
        $arrow_index = $i;
        last;
      }
      $paren_depth++ if $token->{value} eq '(';
      $paren_depth-- if $token->{value} eq ')';
      $bracket_depth++ if $token->{value} eq '[';
      $bracket_depth-- if $token->{value} eq ']';
      $brace_depth++ if $token->{value} eq '{';
      $brace_depth-- if $token->{value} eq '}';
      fail "canonical_command function is malformed" if $paren_depth < 0 || $bracket_depth < 0 || $brace_depth < 0;
    }
  }

  fail "canonical_command function is malformed" if $arrow_index < 0;

  my @lhs = @$arm[0 .. $arrow_index - 1];
  my @rhs = @$arm[$arrow_index + 1 .. $#$arm];

  if (@lhs == 1 && @rhs == 1 && $lhs[0]->{kind} eq 'ident' && $rhs[0]->{kind} eq 'ident') {
    return { passthrough => 1 } if $lhs[0]->{value} eq 'other' && $rhs[0]->{value} eq 'other';
    fail "canonical_command function is malformed";
  }

  my @aliases;
  my $expect_literal = 1;
  for my $token (@lhs) {
    if ($expect_literal) {
      fail "canonical_command function is malformed" unless $token->{kind} eq 'string' || $token->{kind} eq 'rawstring';
      push @aliases, literal_value($token);
      $expect_literal = 0;
      next;
    }
    fail "canonical_command function is malformed" unless token_is($token, 'punct', '|');
    $expect_literal = 1;
  }

  fail "canonical_command function is malformed" if $expect_literal || !@aliases;
  fail "canonical_command function is malformed" unless @rhs == 1;
  my $canonical_token = $rhs[0];
  fail "canonical_command function is malformed" unless $canonical_token->{kind} eq 'string' || $canonical_token->{kind} eq 'rawstring';

  my $canonical = literal_value($canonical_token);
  return { pairs => [ map { "$_\t$canonical" } @aliases ] };
}

my $mode = shift @ARGV // fail "usage: parse-rust-surface.pl MODE FILE";
my $source_file = shift @ARGV // fail "usage: parse-rust-surface.pl MODE FILE";
open my $fh, '<', $source_file or fail "open($source_file): $!";
local $/;
my $text = <$fh>;
defined $text or fail "read($source_file): $!";
my $tokens = tokenize_rust($text);

if ($mode eq 'commands') {
  my @commands = extract_command_rows($tokens);
  my %seen;
  print join("\n", sort grep { !$seen{$_}++ } @commands), "\n";
  exit 0;
}

if ($mode eq 'aliases') {
  my ($body_start, $body_end) = extract_function_body($tokens);
  my @pairs = extract_alias_pairs($tokens, $body_start, $body_end);
  my %seen_alias;
  my @unique;
  for my $pair (@pairs) {
    my ($alias, $canonical) = split /\t/, $pair, 2;
    fail "duplicate canonical_command alias" if exists $seen_alias{$alias};
    $seen_alias{$alias} = $canonical;
    push @unique, $pair;
  }
  print join("\n", sort @unique), "\n";
  exit 0;
}

fail "unknown mode: $mode";
