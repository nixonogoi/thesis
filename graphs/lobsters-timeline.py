#!/usr/bin/env python3

import common
import matplotlib
import matplotlib.pyplot as plt
import pandas as pd
import sys

d = common.load('lobsters', only_good = False)
d = d.query('op == "all" & scale == 2000 & memlimit == 0.125 & partial == True')
d = d.query('until <= 128 & metric == "sojourn" & durable == False')
d = d.reset_index().set_index("partial")
colors = {
    'sojourn': ['#e34a33', '#fdbb84', '#fee8c8'],
}
fig, ax = plt.subplots()
colors = common.memlimit_colors(5, True)

# inject a point at until=0 that's == until=1, to make until=1 more visible
zero = d.query('until == 1').copy()
zero['until'] = 0
d = pd.concat([zero, d])

ax.fill_between(d["until"], d["p25"], d["median"], step="pre", color=colors[0], label="25\\%--50\\%")
ax.fill_between(d["until"], d["median"], d["p90"], step="pre", color=colors[1], label="50\\%--90\\%")
ax.fill_between(d["until"], d["p90"], d["p95"], step="pre", color=colors[2], label="90\\%--95\\%")
ax.fill_between(d["until"], d["p95"], d["p99"], step="pre", color=colors[3], label="95\\%--99\\%")
ax.fill_between(d["until"], d["p99"], d["max"], step="pre", color=colors[4], label="99\\%--Max")
ax.step(
    d["until"],
    d["mean"],
    '.-',
    where="pre",
    label="Mean",
    color="black",
    markerfacecolor="white",
    marker="<",
    ms=7
)

ax.set_ylabel('Page latency')
ax.set_xlabel('Time after start [s]')
ax.set_xlim(0.5, 128)
ax.set_ylim(1, 50000)
ax.set_xscale('log', base=2)
ax.set_xticks([1, 2, 4, 8, 16, 32, 64, 128])
ax.set_yscale('log')
ax.set_yticks([1, 10, 100, 1000])
ax.set_yticklabels(["1ms", "10ms", "100ms", "1s"])
# use normal numbers on x axis
ax.xaxis.set_major_formatter(matplotlib.ticker.ScalarFormatter())
ax.xaxis.get_major_formatter().set_scientific(False)
ax.xaxis.get_major_formatter().set_useOffset(False)

# make the legend be in a sane order
# (increasing tail across, not down, mean at the end)
handles, labels = ax.get_legend_handles_labels()
order = [1, 4, 2, 5, 3, 0]
plt.legend([handles[i] for i in order], [labels[i] for i in order], loc='upper center', ncol=3)

fig.tight_layout()
plt.savefig("{}.pdf".format(sys.argv[1]), format="pdf")
# for thesis presentation backup slides:
# plt.savefig("{}.png".format(sys.argv[1]), format="png", dpi=256)
