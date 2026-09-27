//! Preserve Go's in-place pdqsort ordering for raster tracing and graph rows.
//!
//! Adapted from Go src/sort/zsortfunc.go (Copyright 2022 The Go Authors).
//! See ../liteparse/licenses/Go-JPEG-LICENSE for the Go BSD license. Equal Hough votes must retain
//! the same partition order because peak suppression depends on that order.

const MAX_INSERTION: usize = 12;
const MAX_PARTIAL_STEPS: usize = 5;
const SHORTEST_SHIFTING: usize = 50;
const SHORTEST_NINTHER: usize = 50;
const MAX_PIVOT_SWAPS: usize = 12;
const PATTERN_MIN_LENGTH: usize = 8;
const BALANCE_DIVISOR: usize = 8;
const XORSHIFT_LEFT_FIRST: u32 = 13;
const XORSHIFT_RIGHT: u32 = 7;
const XORSHIFT_LEFT_LAST: u32 = 17;

#[derive(Clone, Copy, PartialEq)]
enum Hint {
    Unknown,
    Increasing,
    Decreasing,
}

pub(crate) fn sort_by_less<T>(data: &mut [T], less: impl Fn(&T, &T) -> bool) {
    let limit = (usize::BITS - data.len().leading_zeros()) as usize;
    pdqsort(data, 0, data.len(), limit, &less);
}

fn insertion_sort<T, F: Fn(&T, &T) -> bool>(data: &mut [T], a: usize, b: usize, less: &F) {
    for i in a + 1..b {
        let mut j = i;
        while j > a && less(&data[j], &data[j - 1]) {
            data.swap(j, j - 1);
            j -= 1;
        }
    }
}

fn sift_down<T, F: Fn(&T, &T) -> bool>(
    data: &mut [T],
    mut root: usize,
    hi: usize,
    first: usize,
    less: &F,
) {
    loop {
        let mut child = 2 * root + 1;
        if child >= hi {
            break;
        }
        if child + 1 < hi && less(&data[first + child], &data[first + child + 1]) {
            child += 1;
        }
        if !less(&data[first + root], &data[first + child]) {
            return;
        }
        data.swap(first + root, first + child);
        root = child;
    }
}

fn heap_sort<T, F: Fn(&T, &T) -> bool>(data: &mut [T], a: usize, b: usize, less: &F) {
    let hi = b - a;
    for i in (0..=(hi - 1) / 2).rev() {
        sift_down(data, i, hi, a, less);
    }
    for i in (0..hi).rev() {
        data.swap(a, a + i);
        sift_down(data, 0, i, a, less);
    }
}

fn pdqsort<T, F: Fn(&T, &T) -> bool>(
    data: &mut [T],
    mut a: usize,
    mut b: usize,
    mut limit: usize,
    less: &F,
) {
    let mut was_balanced = true;
    let mut was_partitioned = true;
    loop {
        let length = b - a;
        if length <= MAX_INSERTION {
            insertion_sort(data, a, b, less);
            return;
        }
        if limit == 0 {
            heap_sort(data, a, b, less);
            return;
        }
        if !was_balanced {
            break_patterns(data, a, b);
            limit -= 1;
        }
        let (mut pivot, mut hint) = choose_pivot(data, a, b, less);
        if hint == Hint::Decreasing {
            data[a..b].reverse();
            pivot = (b - 1) - (pivot - a);
            hint = Hint::Increasing;
        }
        if was_balanced
            && was_partitioned
            && hint == Hint::Increasing
            && partial_insertion_sort(data, a, b, less)
        {
            return;
        }
        if a > 0 && !less(&data[a - 1], &data[pivot]) {
            a = partition_equal(data, a, b, pivot, less);
            continue;
        }
        let (mid, partitioned) = partition(data, a, b, pivot, less);
        was_partitioned = partitioned;
        let left_len = mid - a;
        let right_len = b - mid;
        let balance_threshold = length / BALANCE_DIVISOR;
        if left_len < right_len {
            was_balanced = left_len >= balance_threshold;
            pdqsort(data, a, mid, limit, less);
            a = mid + 1;
        } else {
            was_balanced = right_len >= balance_threshold;
            pdqsort(data, mid + 1, b, limit, less);
            b = mid;
        }
    }
}

fn partition<T, F: Fn(&T, &T) -> bool>(
    data: &mut [T],
    a: usize,
    b: usize,
    pivot: usize,
    less: &F,
) -> (usize, bool) {
    data.swap(a, pivot);
    let (mut i, mut j) = (a + 1, b - 1);
    while i <= j && less(&data[i], &data[a]) {
        i += 1;
    }
    while i <= j && !less(&data[j], &data[a]) {
        j -= 1;
    }
    if i > j {
        data.swap(j, a);
        return (j, true);
    }
    data.swap(i, j);
    i += 1;
    j -= 1;
    loop {
        while i <= j && less(&data[i], &data[a]) {
            i += 1;
        }
        while i <= j && !less(&data[j], &data[a]) {
            j -= 1;
        }
        if i > j {
            break;
        }
        data.swap(i, j);
        i += 1;
        j -= 1;
    }
    data.swap(j, a);
    (j, false)
}

fn partition_equal<T, F: Fn(&T, &T) -> bool>(
    data: &mut [T],
    a: usize,
    b: usize,
    pivot: usize,
    less: &F,
) -> usize {
    data.swap(a, pivot);
    let (mut i, mut j) = (a + 1, b - 1);
    loop {
        while i <= j && !less(&data[a], &data[i]) {
            i += 1;
        }
        while i <= j && less(&data[a], &data[j]) {
            j -= 1;
        }
        if i > j {
            break;
        }
        data.swap(i, j);
        i += 1;
        j -= 1;
    }
    i
}

fn partial_insertion_sort<T, F: Fn(&T, &T) -> bool>(
    data: &mut [T],
    a: usize,
    b: usize,
    less: &F,
) -> bool {
    let mut i = a + 1;
    for _ in 0..MAX_PARTIAL_STEPS {
        while i < b && !less(&data[i], &data[i - 1]) {
            i += 1;
        }
        if i == b {
            return true;
        }
        if b - a < SHORTEST_SHIFTING {
            return false;
        }
        data.swap(i, i - 1);
        if i - a >= 2 {
            for j in (1..i).rev() {
                if !less(&data[j], &data[j - 1]) {
                    break;
                }
                data.swap(j, j - 1);
            }
        }
        if b - i >= 2 {
            for j in i + 1..b {
                if !less(&data[j], &data[j - 1]) {
                    break;
                }
                data.swap(j, j - 1);
            }
        }
    }
    false
}

fn break_patterns<T>(data: &mut [T], a: usize, b: usize) {
    let length = b - a;
    if length < PATTERN_MIN_LENGTH {
        return;
    }
    let mut random = length as u64;
    let modulus = 1usize << (usize::BITS - length.leading_zeros());
    let middle = a + (length / 4) * 2;
    for index in middle - 1..=middle + 1 {
        random ^= random << XORSHIFT_LEFT_FIRST;
        random ^= random >> XORSHIFT_RIGHT;
        random ^= random << XORSHIFT_LEFT_LAST;
        let mut other = random as usize & (modulus - 1);
        if other >= length {
            other -= length;
        }
        data.swap(index, a + other);
    }
}

fn order_two<T, F: Fn(&T, &T) -> bool>(
    data: &[T],
    a: usize,
    b: usize,
    swaps: &mut usize,
    less: &F,
) -> (usize, usize) {
    if less(&data[b], &data[a]) {
        *swaps += 1;
        (b, a)
    } else {
        (a, b)
    }
}

fn median<T, F: Fn(&T, &T) -> bool>(
    data: &[T],
    a: usize,
    b: usize,
    c: usize,
    swaps: &mut usize,
    less: &F,
) -> usize {
    let (a, b) = order_two(data, a, b, swaps, less);
    let (b, _) = order_two(data, b, c, swaps, less);
    let (_, b) = order_two(data, a, b, swaps, less);
    b
}

fn choose_pivot<T, F: Fn(&T, &T) -> bool>(
    data: &[T],
    a: usize,
    b: usize,
    less: &F,
) -> (usize, Hint) {
    let length = b - a;
    let mut swaps = 0;
    let (mut i, mut j, mut k) = (a + length / 4, a + length / 4 * 2, a + length / 4 * 3);
    if length >= PATTERN_MIN_LENGTH {
        if length >= SHORTEST_NINTHER {
            i = median(data, i - 1, i, i + 1, &mut swaps, less);
            j = median(data, j - 1, j, j + 1, &mut swaps, less);
            k = median(data, k - 1, k, k + 1, &mut swaps, less);
        }
        j = median(data, i, j, k, &mut swaps, less);
    }
    let hint = match swaps {
        0 => Hint::Increasing,
        MAX_PIVOT_SWAPS => Hint::Decreasing,
        _ => Hint::Unknown,
    };
    (j, hint)
}
