+++
title = "AI is Still Garbage at Complex Reasoning, But Here's How to Make It Work"
date = 2025-07-30
description = "The real story of AI collaboration: where it fails, where it succeeds, and how to actually leverage it for 10x productivity gains."
authors = ["Dario Lencina Talarico"]
slug = "your-brain-has-token-exhaustion-here-is-how-fix-it"
tags = ["ai", "productivity", "cursor", "claude", "software-development", "ai-limitations", "ai-productivity"]
categories = ["Software Development", "AI", "Productivity"]
keywords = ["ai", "cursor", "claude", "productivity", "software-development", "ai-limitations"]
[taxonomies]
tags = ["ai", "productivity", "cursor", "claude", "software-development", "ai-limitations"]
authors = ["Dario Lencina Talarico"]
+++


Before:

impl Solution {
    pub fn find_median_sorted_arrays(nums1: Vec<i32>, nums2: Vec<i32>) -> f64 {
        let mut output = vec!();
        let mut i_ptr = 0;
        let mut j_ptr = 0;
        while i_ptr < nums1.len() || j_ptr < nums2.len() {
            let next_nums1 = nums1.get(i_ptr);
            let next_nums2 = nums2.get(j_ptr);
            if next_nums1.is_some() && next_nums2.is_some() && next_nums1.unwrap() < next_nums2.unwrap() {
                output.push(next_nums1.unwrap());
                i_ptr +=1;
            } else if next_nums1.is_some() && next_nums2.is_some() && next_nums2.unwrap() < next_nums1.unwrap() {
                output.push(next_nums2.unwrap());
                j_ptr +=1;
            } else {
                if let Some(next_nums1) = next_nums1 {
                    output.push(next_nums1);
                    i_ptr += 1;
                }
                if let Some(next_nums2) = next_nums2 {
                    output.push(next_nums2);
                    j_ptr += 1;
                }
            }
        }

        let len = output.len();
        if len == 1 {
            return *output[0] as f64;
        }
        if len % 2 != 0 {
            let index = (len / 2);
            return *output[index] as f64
        } else {
            let index = len / 2;
            let index_2 = len / 2 - 1;
            return (*output[index] as f64 + *output[index_2] as f64) / 2f64
        }        
    }
}


With pattern matching:

```
impl Solution {
    pub fn find_median_sorted_arrays(nums1: Vec<i32>, nums2: Vec<i32>) -> f64 {
        let mut output = vec!();
        let mut i_ptr = 0;
        let mut j_ptr = 0;
        while i_ptr < nums1.len() || j_ptr < nums2.len() {
            match (nums1.get(i_ptr), nums2.get(j_ptr)) {
                (Some(&x), Some(&y)) if x <= y => { output.push(x); i_ptr += 1; }
                (Some(_),  Some(&y))           => { output.push(y); j_ptr += 1; }
                (Some(&x), None)               => { output.push(x); i_ptr += 1; }
                (None,      Some(&y))          => { output.push(y); j_ptr += 1; }
                (None,      None)              => break,
            }
        }

        let len = output.len();
        if len == 1 {
            return output[0] as f64;
        }
        if len % 2 != 0 {
            let index = (len / 2);
            return output[index] as f64
        } else {
            let index = len / 2;
            let index_2 = len / 2 - 1;
            return (output[index] as f64 + output[index_2] as f64) / 2f64
        }        
    }
}
```

Find the longest palindrome in a string:

> There's a bug in the slice!!
```
mpl Solution {

    pub fn is_palindrome(s: &str) -> bool {
        s.chars().eq(s.chars().rev())
    }

    pub fn longest_palindrome(s: String) -> String {
        // we need to find the longest substring that is a palindrome, the rule is:
        // substring == substring.rev()
        let mut longest: Option<String> = None;

        // how do we slice the string to check for palindromes?
        // babad
        // 
        // we could start from the left, and try to find palindromes first, we can try to create 
        // slices and test for the palindrome condition

        // all we need to do is feed all possible strings to is_palindrome and keep the longest.
        // we can achieve this will slices or loops, idk
        for i in 0..s.len() {
            for j in 1..s.len() {
                if i >= j {
                    continue;
                }
                let substring = &s[i..j];
                if Self::is_palindrome(substring) {
                     match longest {
                         None => {
                             longest = Some(substring.into());
                         }
                         Some(s) if s.len() < substring.len() => {
                             longest = Some(substring.into());
                         },
                         _ => {}
                     }
                }
            }
        }
        
        longest.unwrap_or_default()
    }
}

```


> This works!!!

```
impl Solution {

    pub fn is_palindrome(s: &str) -> bool {
        s.chars().eq(s.chars().rev())
    }

    pub fn longest_palindrome(s: String) -> String {
        // we need to find the longest substring that is a palindrome, the rule is:
        let mut longest: Option<String> = None;

        // how do we slice the string to check for palindromes?
        // babad
        // 
        // we could start from the left, and try to find palindromes first, we can try to create 
        // slices and test for the palindrome condition

        // all we need to do is feed all possible strings to is_palindrome and keep the longest.
        // we can achieve this will slices or loops, idk

        if s.len() == 1 {
            return s;
        }
        //bb
        for i in 0..=s.len() {
            for j in 0..=s.len() {
                if i >= j {
                    continue;
                }
                let substring = &s[i..j];
                if Self::is_palindrome(substring) {
                     match longest {
                         None => {
                             longest = Some(substring.into());
                         }
                         Some(s) if s.len() < substring.len() => {
                             longest = Some(substring.into());
                         },
                         _ => {}
                     }
                }
            }
        }
        
        longest.unwrap_or_default()
    }
}
```

The best:

```
impl Solution {

    pub fn is_palindrome(s: &str) -> bool {
        s.chars().eq(s.chars().rev())
    }

    pub fn longest_palindrome(s: String) -> String {
        let mut longest: Option<String> = None;

        for i in 0..=s.len() {
            for j in 0..=s.len() {
                if i >= j {
                    continue;
                }
                let substring = &s[i..j];
                if Self::is_palindrome(substring) {
                     match longest {
                         None => {
                             longest = Some(substring.into());
                         }
                         Some(s) if s.len() < substring.len() => {
                             longest = Some(substring.into());
                         },
                         _ => {}
                     }
                }
            }
        }
        
        longest.unwrap_or_default()
    }
}
```

Reverse number my solution:
```
impl Solution {
    pub fn reverse(x: i32) -> i32 {
        let is_neg = x < 0;
        let stringo = format!("{x}");
        let charingo = stringo.chars().rev();

        // Remove sign
        let charingo = charingo.filter(|char| *char != '-');

        let printingo: String = charingo.collect(); 

        let mut result: i64 = printingo.parse().unwrap();
        // If number was originally negative put the sign back
        result = if is_neg {
            result * -1
        }  else {
            result
        };

        // Return 0 if it doesn't fit in i32
        if result < i32::MIN as i64 || result > i32::MAX as i64 {
            return 0;
        }

        result as i32
    }
}
```


Chatgpt solution:
```
impl Solution {
    pub fn reverse(mut x: i32) -> i32 {
        let mut rev: i32 = 0;

        while x != 0 {
            let digit = x % 10;   // pop
            x /= 10;              // shrink

            // rev = rev * 10 + digit, with overflow checks
            let tmp = match rev.checked_mul(10) {
                Some(v) => v,
                None => return 0,  // would overflow i32
            };
            rev = match tmp.checked_add(digit) {
                Some(v) => v,
                None => return 0,  // would overflow i32
            };
        }

        rev
    }
}
```


Longest subarray:

slow:
```
impl Solution {
    pub fn longest_subarray(nums: Vec<i32>) -> i32 {
        let mut left: usize = 0;
        let mut zeros: usize = 0;
        let mut best: usize = 0;

        for right in 0..nums.len() {
            if nums[right] == 0 {
                zeros += 1;
            }

            // Shrink until we have at most one zero in window [left..=right]
            while zeros > 1 {
                if nums[left] == 0 {
                    zeros -= 1;
                }
                left += 1;
            }

            // window length with ≤1 zero
            // we’re updating best with the length of the current inclusive window 
            // after we’ve shrunk it so it has at most one zero.
            best = best.max(right - left + 1);
        }

        // Must delete one element
        if best == 0 { 0 } else { (best as i32) - 1 }
    }
}
```

Spiral matrix:

```
impl Solution {
    pub fn spiral_order(matrix: Vec<Vec<i32>>) -> Vec<i32> {
        let mut top_left_ptr = (0, 0);
        let mut top_right_ptr = (matrix[0].len() - 1, 0);
        let mut bottom_right_ptr = (matrix[0].len() - 1, matrix.len() - 1);
        let mut bottom_left_ptr = (0, matrix.len() - 1);
        let mut acc = vec![];

        let target_length = matrix.len() * matrix[0].len();
        while acc.len() < target_length {
            // top row (left -> right-1)
            for i in top_left_ptr.0..top_right_ptr.0 {
                if let Some(v) = matrix.get(top_left_ptr.1).and_then(|row| row.get(i)) {
                    acc.push(*v);
                    if acc.len() == target_length { return acc; }
                } else { return acc; }
            }

            // right column (top -> bottom)
            for i in top_right_ptr.1..=bottom_right_ptr.1 {
                if let Some(v) = matrix.get(i).and_then(|row| row.get(top_right_ptr.0)) {
                    acc.push(*v);
                    if acc.len() == target_length { return acc; }
                } else { return acc; }
            }

            // move top-left inward
            top_left_ptr = (top_left_ptr.0 + 1, top_left_ptr.1 + 1);

            // bottom row (right-1 -> left) on bottom_right_ptr.1
            for i in (bottom_left_ptr.0..bottom_right_ptr.0).rev() {
                if let Some(v) = matrix.get(bottom_right_ptr.1).and_then(|row| row.get(i)) {
                    acc.push(*v);
                    if acc.len() == target_length { return acc; }
                } else { return acc; }
            }

            // move top-right inward
            top_right_ptr = (top_right_ptr.0 - 1, top_right_ptr.1 + 1);

            // FIX 1: bottom-right must move inward *upwards* (row - 1), not +1
            bottom_right_ptr = (bottom_right_ptr.0 - 1, bottom_right_ptr.1 - 1);

            // left column (bottom-1 -> top) on bottom_left_ptr.0
            for i in (top_left_ptr.1..bottom_left_ptr.1).rev() {
                if let Some(v) = matrix.get(i).and_then(|row| row.get(bottom_left_ptr.0)) {
                    acc.push(*v);
                    if acc.len() == target_length { return acc; }
                } else { return acc; }
            }

            // FIX 2: bottom-left must move inward *upwards* (row - 1), not derived from bottom_right_ptr
            bottom_left_ptr = (bottom_left_ptr.0 + 1, bottom_left_ptr.1 - 1);
        }
        acc
    }
}
```


Area of max diagonal:

Bad
```
impl Solution {
    pub fn area_of_max_diagonal(dimensions: Vec<Vec<i32>>) -> i32 {
        let mut result: Vec<(i32, i32)> = dimensions.iter().map(|rectangle| {
            let l = *rectangle.get(0).unwrap_or(&0);
            let w = *rectangle.get(1).unwrap_or(&0);
            let diag_len = (l * l + w * w); // no need to get sqrt
            let area = l * w;
            (diag_len, area)
        }).collect();
        result.sort_by(|(ad2, aa), (bd2, ba)| bd2.cmp(ad2).then_with(|| ba.cmp(aa)));
        result.first().unwrap_or(&(0, 0)).1 
    }
}

```

Good:
```
impl Solution {
    pub fn area_of_max_diagonal(dimensions: Vec<Vec<i32>>) -> i32 {
        dimensions.into_iter()
            .map(|v| {
                let l = *v.get(0).unwrap_or(&0) as i64;
                let w = *v.get(1).unwrap_or(&0) as i64;
                (l*l + w*w, l*w) // (diag², area)
            })
            .max()                       // lexicographic: diag², then area
            .map(|(_, area)| area as i32)
            .unwrap_or(0)
    }
}
```


Slow:
```
impl Solution {
    pub fn minimum_rounds(tasks: Vec<i32>) -> i32 {
        // In each round, you can complete either 2 or 3 tasks of the same dificulty level
        // Return the minimum rounds required to complete all the tasks or -1 if it is not possible to
        // complete all the tasks
        // first sort ascending
        let mut tasks = tasks;
        tasks.sort();

        // prior to slicing, we need to test if the first two or 3 elements are equal, else it is the end of the road for this array.

        let a = tasks.get(0);
        let b = tasks.get(1);
        let c = tasks.get(2);

        let can_pull_2_tasks = a.is_some() && b.is_some() && (a == b);
        let can_pull_3_tasks = can_pull_2_tasks && c.is_some() && a == c;

        let min_rounds_2 = if can_pull_2_tasks { Self::min_rounds_helper(&tasks[2..], 1) } else {None };
        let min_rounds_3 = if can_pull_3_tasks { Self::min_rounds_helper(&tasks[3..], 1) } else { None };
        
        let min = match (min_rounds_2, min_rounds_3) {
            (Some(a), Some(b)) => {
                Some(a.min(b))
            },
            (Some(a), None) => {
                Some(a)
            },
            (None, Some(a)) => {
                Some(a)
            },
            _ => None
        };
        min.unwrap_or(-1)
    }

    pub fn min_rounds_helper(tasks: &[i32], rounds: i32) -> Option<i32> {
        if tasks.is_empty() {
            return Some(rounds);
        }
        let a = tasks.get(0);
        let b = tasks.get(1);
        let c = tasks.get(2);

        let can_pull_2_tasks = a.is_some() && b.is_some() && (a == b);
        let can_pull_3_tasks = can_pull_2_tasks && c.is_some() && a == c;

        if !can_pull_2_tasks && !can_pull_3_tasks {
            return None;
        }

        let min_rounds_2 = if can_pull_2_tasks { Self::min_rounds_helper(&tasks[2..], rounds + 1) } else { None };
        let min_rounds_3 = if can_pull_3_tasks { Self::min_rounds_helper(&tasks[3..], rounds + 1) } else { None };
        
        match (min_rounds_2, min_rounds_3) {
            (Some(a), Some(b)) => {
                Some(a.min(b))
            },
            (Some(a), None) => {
                Some(a)
            },
            (None, Some(a)) => {
                Some(a)
            },
            _ => None
        }
    }
}
```


faster
```
use std::collections::HashMap;

impl Solution {
    pub fn minimum_rounds(tasks: Vec<i32>) -> i32 {
        let mut freq: HashMap<i32, i32> = HashMap::new();
        for t in tasks {
            *freq.entry(t).or_insert(0) += 1;
        }

        let mut rounds: i32 = 0;
        for &f in freq.values() {
            if f == 1 {
                return -1; // can't make a group of 2 or 3
            }
            rounds += (f + 2) / 3; // ceil(f / 3)
        }
        rounds
    }
}
```

islands:

```
impl Solution {
    pub fn num_islands(grid: Vec<Vec<char>>) -> i32 {
        let mut visited = vec![vec![false; grid[0].len()]; grid.len()];
        // we are going to visit the map and collect the number of islands as we go. 

        let mut island_counter = 0;
        // we will start scanning for an island, once we find one, we will do dfs on it, meaning, explore the 
        // entire island before moving forward to the next island, we will mark the visited coordinates 
        // using the visited matrix.
        
        for (x, row) in grid.iter().enumerate() {
            for (y, value) in row.iter().enumerate() {
                if visited[x][y] {
                    continue;
                }
                visited[x][y] = true;
                if *value == '1' {
                    island_counter += 1;
                    // Found an island, DFS in it to find all the coordinates.
                    // we can move up, left, right, down, we are done until we explore the complete
                    // island
                    let mut stack = vec!();
                    stack.push((x-1, y));
                    stack.push((x+1, y));
                    stack.push((x, y-1));
                    stack.push((x, y+1));
                    while let Some((x, y)) = stack.pop() {
                        if let Some(value) = grid.get(x).and_then(|x_value| x_value.get(y)) {
                            if !visited[x][y] && *value == '1' {
                                visited[x][y] = true;
                                stack.push((x-1, y));
                                stack.push((x+1, y));
                                stack.push((x, y-1));
                                stack.push((x, y+1));
                            } else {
                                continue;
                            }
                        }
                    }
                }
            }
        }

        island_counter
    }
}
```

optimized:


Search Suggestion system:

bad:
```
impl Solution {
    pub fn suggested_products(products: Vec<String>, search_word: String) -> Vec<Vec<String>> {
        let mut products = products;
        products.sort();
        let mut results = vec!();
        for i in 1..=search_word.len() {
            let sliced_input: String = search_word.chars().take(i).collect();
            let filter_results: Vec<String> = products.iter().filter(|product| {
                // compare substring
                let product_prefix: String = product.chars().take(i).collect();
                sliced_input == product_prefix
            }).map(|slice| slice.to_string()).take(3).collect();
            results.push(filter_results);
        }
        results
    }
}
```

enhanced:

impl Solution {
    pub fn suggested_products(mut products: Vec<String>, search_word: String) -> Vec<Vec<String>> {
        products.sort(); // lexicographic

        let mut res = Vec::with_capacity(search_word.len());
        let mut prefix = String::new();

        // Start with the full range; shrink it each step.
        let mut lo = 0usize;
        let mut hi = products.len();

        for ch in search_word.chars() {
            prefix.push(ch);

            // Work only inside the previous window [lo, hi)
            let base = lo;
            let window = &products[lo..hi];

            // lower bound for prefix
            let l_rel = window.partition_point(|s| s < &prefix);

            // upper bound for strings starting with prefix
            let mut hi_key = prefix.clone();
            hi_key.push('{'); // next ASCII after 'z' (inputs are lowercase)
            let h_rel = window.partition_point(|s| s < &hi_key);

            // Update window to the narrower range
            lo = base + l_rel;
            hi = base + h_rel;

            // Take up to 3 from the new window
            let take = (hi - lo).min(3);
            let mut picked = Vec::with_capacity(take);
            for s in &products[lo..lo + take] {
                picked.push(s.clone());
            }
            res.push(picked);
        }

        res
    }
}


Slow

impl Solution {
    pub fn min_meeting_rooms(mut intervals: Vec<Vec<i32>>) -> i32 {
        // This is similar to the clips that we tackled earlier. 
        // now we know that the first order of business is to sort the intervals
        intervals.sort();

        let mut required_rooms = 0;

        // for the brute force solution, we need to check how many meetings are taking 
        // place during the whole interval, from day_start to day_end

        let day_start = intervals[0][0];
        let day_end = intervals.last().unwrap()[1];
        let mut visiting_ptr = day_start;

        if intervals.len() == 1 {
            return 1;
        }

        for i in (day_start..=day_end) {
            // for each time slot, we need to query all meetings taking place. 
            let mut meetings_taking_place_at_i = 0;
            for (j, meeting) in intervals.iter().enumerate() {
                let m_start = meeting[0];
                let m_end = meeting[1];
                if m_start < i && m_end >= i {
                    meetings_taking_place_at_i += 1;
                }
                required_rooms = required_rooms.max(meetings_taking_place_at_i);
                if m_start > i {
                    break;
                }
            }
        }

        required_rooms
    }
}

fast 

```
impl Solution {
    pub fn min_meeting_rooms(intervals: Vec<Vec<i32>>) -> i32 {
        let n = intervals.len();
        if n == 0 { return 0; }
        if n == 1 { return 1; }

        let mut starts: Vec<i32> = intervals.iter().map(|m| m[0]).collect();
        let mut ends:   Vec<i32> = intervals.iter().map(|m| m[1]).collect();
        starts.sort_unstable();
        ends.sort_unstable();

        let (mut i, mut j) = (0usize, 0usize);
        let (mut curr, mut best) = (0i32, 0i32);

        while i < n {
            // If a meeting starts before the earliest ending one finishes,
            // we need another room. (Use <, not <=, so start==end reuses a room.)
            if starts[i] < ends[j] {
                curr += 1;
                best = max(best, curr);
                i += 1;
            } else {
                // A meeting ended; free a room.
                curr -= 1;
                j += 1;
            }
        }
        best
    }
}
```

bad

```
impl Solution {
    pub fn group_anagrams(strs: Vec<String>) -> Vec<Vec<String>> {
        let mut anagrams: HashMap<String, Vec<String>> = HashMap::new();
        
        // sort all letters in each word ascending
        let sorted_str: Vec<String> = strs.iter().map(|word| {
            let mut sorted: Vec<char> = word.chars().collect();
            sorted.sort();
            sorted.into_iter().collect()
        }).collect();

        for (i, word) in sorted_str.iter().enumerate() {
            anagrams.entry(word.to_string()).or_insert_with(Vec::new).push(strs[i].clone());
        }
        let all_values: Vec<Vec<String>> = anagrams.values().cloned().collect();
        all_values
    }
}
```

good:
```
use std::collections::HashMap;

impl Solution {
    pub fn group_anagrams(strs: Vec<String>) -> Vec<Vec<String>> {
        let mut anagrams: HashMap<String, Vec<String>> = HashMap::new();

        for (i, word) in strs.iter().enumerate() {
            let mut chars: Vec<char> = word.chars().collect();
            chars.sort_unstable();
            let key: String = chars.into_iter().collect();
            anagrams.entry(key).or_insert_with(Vec::new).push(word.to_string());
        }
        anagrams.into_values().collect()
    }
}
```



bad

```
impl Solution {
    pub fn merge(mut intervals: Vec<Vec<i32>>) -> Vec<Vec<i32>> {
        // as we know, these problems are always solved more easily if we sort the intervals
        intervals.sort();

        let mut n = intervals.len();
        let mut adjacency_list: Vec<Vec<usize>> = vec![Vec::new(); n];

        // build neighbors
        for left_idx in 0..n {
            let left_end = intervals[left_idx][1];
            for right_idx in (left_idx + 1)..n {
                let right_start = intervals[right_idx][0];
                // Since starts are nondecreasing, once right_start > left_end,
                // no later interval can overlap left_idx.
                if right_start > left_end {
                    break;
                }

                if Self::intervals_overlap(&intervals[left_idx], &intervals[right_idx]) {
                    adjacency_list[left_idx].push(right_idx);
                    adjacency_list[right_idx].push(left_idx);
                }
            }
        }

        // Step 3. DFS to find connected components
        let mut visited = vec![false; n];
        let mut merged = vec!();

        for start_idx in 0..n {
            if visited[start_idx] {
                continue;
            }

            let mut stack = vec![start_idx];
            visited[start_idx] = true;
            let mut group = Vec::new();

            while let Some(node_idx) = stack.pop() {
                group.push(node_idx);
                for neighbor_idx in adjacency_list[node_idx].iter() {
                    let neighbor_idx = neighbor_idx.clone();
                    if !visited[neighbor_idx] { 
                        visited[neighbor_idx] = true;
                        stack.push(neighbor_idx);
                    }
                }
            }

            // reduce one component to [min_start, max_end]
            let mut min_start = i32::MAX;
            let mut max_end = i32::MIN;

            for idx in group {
                min_start = min_start.min(intervals[idx][0]);
                max_end = max_end.max(intervals[idx][1]);
            }
            merged.push(vec![min_start, max_end]);
        }
        merged
    }

    fn intervals_overlap(a: &[i32], b: &[i32]) -> bool {
        a[0] <= b[1] && b[0] <= a[1]
    }
}
```

video clips bad:

impl Solution {
    pub fn video_stitching(mut clips: Vec<Vec<i32>>, time: i32) -> i32 {
        // We take ownership of `clips` and mark it `mut` so we can sort it.

        clips.sort_unstable_by(|a, b| 
            a[0].cmp(&b[0]).then_with(|| a[1].cmp(&b[1]))
        );
        // Sort clips by start time ascending; if starts tie, by end ascending.
        // After this, all clips that start earlier come first. This lets us
        // scan once from left to right.

        let n = clips.len();
        let mut i = 0_usize;  // index while scanning the sorted clips
        let mut used = 0;     // how many clips we've "committed" to use
        let mut cur_end = 0;  // we currently cover [0, cur_end]
        let mut far_end = 0;  // the farthest end we can reach from this layer

        while cur_end < time {
            // While we can, absorb every clip that starts at/before `cur_end`
            // and track the farthest end any of them can reach.
            while i < n && clips[i][0] <= cur_end {
                far_end = far_end.max(clips[i][1]);
                i += 1;
            }

            // If we couldn't push `far_end` past `cur_end`, there's a gap
            // (nothing starts early enough to keep covering continuously).
            if far_end == cur_end {
                return -1;
            }

            // "Commit" one jump: we choose the set of clips we just scanned
            // (conceptually, the best one among them) to extend coverage.
            used += 1;
            cur_end = far_end;

            // If we now cover at least `time`, we're done.
            if cur_end >= time {
                return used;
            }
            // Otherwise loop again and try to extend from the new frontier.
        }

        used
    }
}

