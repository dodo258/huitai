# 压力测试用的假 CLI（由 tests/stress/main.rs 写到临时目录后用 perl 执行）。
#   fast <每秒字节> <停止文件> <标记> <行数>：按速率持续输出彩色、中文宽字、折行长行、标题和偶尔的响铃；
#       停止文件出现后清屏画一屏固定的 FINAL 画面、设标题，然后一直空闲（不读输入）。
#   storm <停止文件> <标记>：见下面 storm 分支（改窗口风暴用）。
#   queries：一直发终端查询（DA、光标位置），从不读输入。
use strict;
use warnings;
use Time::HiRes qw(time sleep);

binmode STDOUT;

# 写不进去（守护进程没了、PTY 关了）就退出。
sub out {
    my ($s) = @_;
    my $off = 0;
    while ($off < length $s) {
        my $n = syswrite(STDOUT, $s, length($s) - $off, $off);
        exit 0 unless defined $n;
        $off += $n;
    }
}

my ($mode, @arg) = @ARGV;
if ($mode eq 'fast') {
    my ($rate, $stop, $tag, $rows) = @arg;
    my ($n, $sent, $start) = (0, 0, time);
    while (!-e $stop) {
        my $chunk = '';
        for (1 .. 64) {
            $n++;
            my $c = 31 + $n % 7;
            my $k = $n % 256;
            $chunk .= "\e[1;${c}m[$tag]\e[0m line $n \xe4\xb8\xad\xe6\x96\x87\xe5\xae\xbd\xe5\xad\x97 "
              . "\e[38;5;${k}mcolor $k\e[0m \e[48;2;30;40;50m bg \e[0m " . ('x' x ($n % 150)) . "\r\n";
        }
        $chunk .= "\e]2;$tag busy $n\a";
        $chunk .= "\a" if $n % 4096 < 64;
        out($chunk);
        $sent += length $chunk;
        my $ahead = $sent / $rate - (time - $start);
        sleep($ahead) if $ahead > 0;
    }
    my $f = "\e[0m\e[H\e[2J";
    for my $i (0 .. $rows - 2) {
        $f .= "\e[3" . ($i % 7 + 1) . "mFINAL $tag row $i\e[0m\r\n";
    }
    $f .= "\e]2;final $tag\a";
    out($f);
    sleep 3600 while 1;
}
elsif ($mode eq 'storm') {
    # storm <停止文件> <标记>：一直输出长短不一的彩色行（改窗口大小期间也不停）；
    # 停止文件出现后清屏，按此刻的窗口大小画 FINAL 画面，最后一行写 SIZE <列> x <行>。
    my ($stop, $tag) = @arg;
    my $n = 0;
    while (!-e $stop) {
        my $chunk = '';
        for (1 .. 32) {
            $n++;
            $chunk .= "\e[3" . ($n % 7 + 1) . "m[$tag] storm $n\e[0m " . ('y' x ($n % 200)) . "\r\n";
        }
        out($chunk);
        sleep 0.002;
    }
    my ($rows, $cols) = split ' ', `stty size`;
    my $f = "\e[0m\e[H\e[2J";
    for my $i (0 .. $rows - 2) {
        $f .= "FINAL $tag row $i\r\n";
    }
    $f .= "SIZE $cols x $rows";
    out($f);
    sleep 3600 while 1;
}
elsif ($mode eq 'queries') {
    while (1) {
        out("\e[c\e[6n");
        sleep 0.001;
    }
}
else {
    die "unknown mode $mode\n";
}
