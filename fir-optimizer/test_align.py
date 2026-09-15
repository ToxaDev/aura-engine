import numpy as np

min_p = np.load('output/fir_30M_minimum_phase.npy')

# Limit it to the first handful of samples for speed
h = min_p[:500000]

sum_h = np.sum(h)
cg = np.sum(np.arange(len(h)) * h) / sum_h

print('Center of gravity (samples):', cg)
