use super::{DIMENSIONS, WorkerError, valid_vector};

pub(super) const MAX_DEPTH: u8 = 32;

/// Visit the left subtree first so floating-point additions occur in source order.
pub(super) fn embed_text<'a>(
    body: &'a str,
    scalars: usize,
    mut embed: impl FnMut(&'a str) -> Result<Vec<f32>, WorkerError>,
) -> Result<Vec<f32>, WorkerError> {
    let mut pending = vec![(body, scalars, 0)];
    // Every leaf contains at least one scalar, and every internal node has two children.
    let mut calls_remaining = 2 * scalars - 1;
    let mut sum = [0.0; DIMENSIONS];
    while let Some((window, count, depth)) = pending.pop() {
        calls_remaining = calls_remaining
            .checked_sub(1)
            .ok_or(WorkerError::InputOverflow)?;
        match embed(window) {
            Ok(vector) => {
                if !valid_vector(&vector) {
                    return Err(WorkerError::Protocol);
                }
                let norm = l2_norm(vector.iter().copied().map(f64::from));
                for (sum, value) in sum.iter_mut().zip(vector) {
                    *sum += (f64::from(value) / norm) * count as f64;
                }
            }
            Err(WorkerError::InputOverflow) if depth < MAX_DEPTH => {
                let (offset, left_count) =
                    split_offset(window, count).ok_or(WorkerError::InputOverflow)?;
                let (left, right) = window.split_at(offset);
                pending.push((right, count - left_count, depth + 1));
                pending.push((left, left_count, depth + 1));
            }
            Err(error) => return Err(error),
        }
    }
    let mean = sum.map(|value| value / scalars as f64);
    let norm = l2_norm(mean.iter().copied());
    if norm == 0.0 || !norm.is_finite() {
        return Err(WorkerError::Inference);
    }
    Ok(mean
        .into_iter()
        .map(|value| (value / norm) as f32)
        .collect())
}

fn l2_norm(values: impl Iterator<Item = f64>) -> f64 {
    values.map(|value| value * value).sum::<f64>().sqrt()
}

fn split_offset(body: &str, count: usize) -> Option<(usize, usize)> {
    let central_half = count.div_ceil(4)..=(count * 3 / 4);
    let mut best = None;
    let mut seen_line_end = false;
    let mut blank_line = true;
    for (left_count, (offset, ch)) in (1..count).zip(body.char_indices()) {
        let after = offset + ch.len_utf8();
        let paragraph_end = match ch {
            '\r' if body.as_bytes().get(after) == Some(&b'\n') => false,
            '\r' | '\n' => {
                let paragraph_end = seen_line_end && blank_line;
                seen_line_end = true;
                blank_line = true;
                paragraph_end
            }
            _ => {
                blank_line &= ch.is_whitespace();
                false
            }
        };
        if central_half.contains(&left_count) {
            let priority = if paragraph_end {
                0
            } else if matches!(ch, '.' | '。' | '!' | '！' | '?' | '？') {
                1
            } else {
                2
            };
            // The tuple breaks equal distances toward the earlier boundary.
            let candidate = (
                priority,
                left_count.abs_diff(count - left_count),
                left_count,
                after,
            );
            best = Some(best.map_or(candidate, |current| candidate.min(current)));
        }
    }
    best.map(|(_, _, left_count, offset)| (offset, left_count))
}
#[cfg(test)]
mod tests {
    use super::*;

    fn axis(index: usize, amplitude: f32) -> Vec<f32> {
        let mut vector = vec![0.0; DIMENSIONS];
        vector[index] = amplitude;
        vector
    }

    #[test]
    fn split_prefers_blank_line_then_sentence_then_scalar() {
        for (body, expected_left) in [
            ("猫\n\n犬鳥。馬羊牛猿虎鹿", "猫\n\n"),
            ("猫\r\n \t\r\n鳥。馬犬羊牛", "猫\r\n \t\r\n"),
            ("猫犬。鳥？馬羊牛", "猫犬。"),
            ("。猫犬鳥馬羊牛猿", "。猫犬鳥"),
            ("猫🐦犬", "猫"),
            ("猫犬", "猫"),
        ] {
            let (offset, count) = split_offset(body, body.chars().count()).unwrap();
            assert_eq!(&body[..offset], expected_left);
            assert_eq!(count, expected_left.chars().count());
        }
        assert_eq!(split_offset("猫", 1), None);
    }

    #[test]
    fn recursive_windows_cover_every_byte_in_order_within_the_call_bound() {
        let body = "猫🐦 aardvark?\r\n\r\n次は犬。\n".repeat(7);
        let scalars = body.chars().count();
        let mut calls = 0;
        let mut leaves = String::new();
        let result = embed_text(&body, scalars, |window| {
            calls += 1;
            if window.chars().count() > 1 {
                return Err(WorkerError::InputOverflow);
            }
            leaves.push_str(window);
            Ok(axis(0, 1.0))
        });
        assert_eq!(result.unwrap(), axis(0, 1.0));
        assert_eq!(leaves.as_bytes(), body.as_bytes());
        assert_eq!(calls, 2 * scalars - 1);
    }

    #[test]
    fn aggregate_normalizes_windows_before_weighting_by_body_scalars() {
        let mut calls = Vec::new();
        let vector = embed_text("猫犬a🐦b", 5, |window| {
            calls.push(window);
            match window {
                "猫犬a🐦b" => Err(WorkerError::InputOverflow),
                "猫犬" => Ok(axis(0, f32::MAX)),
                "a🐦b" => Ok(axis(1, f32::from_bits(1))),
                _ => panic!("unexpected window {window:?}"),
            }
        })
        .unwrap();
        assert_eq!(calls, ["猫犬a🐦b", "猫犬", "a🐦b"]);
        let norm = 13.0_f64.sqrt();
        assert_eq!(&vector[..2], &[(2.0 / norm) as f32, (3.0 / norm) as f32]);
    }

    #[test]
    fn only_overflow_splits_and_late_failures_discard_partial_results() {
        let mut calls = Vec::new();
        let result = embed_text("猫犬鳥馬", 4, |window| {
            calls.push(window);
            match window {
                "猫犬鳥馬" => Err(WorkerError::InputOverflow),
                "猫犬" => Ok(axis(0, 1.0)),
                "鳥馬" => Err(WorkerError::Inference),
                _ => panic!("unexpected window {window:?}"),
            }
        });
        assert_eq!(result, Err(WorkerError::Inference));
        assert_eq!(calls, ["猫犬鳥馬", "猫犬", "鳥馬"]);

        let mut calls = Vec::new();
        let result = embed_text("猫犬", 2, |window| {
            calls.push(window);
            Err(WorkerError::InputOverflow)
        });
        assert_eq!(result, Err(WorkerError::InputOverflow));
        assert_eq!(calls, ["猫犬", "猫"]);
    }

    #[test]
    fn invalid_vectors_and_a_zero_norm_aggregate_fail() {
        for invalid in [
            vec![],
            axis(0, 0.0),
            axis(0, f32::INFINITY),
            axis(0, f32::NAN),
        ] {
            assert_eq!(
                embed_text("猫", 1, |_| Ok(invalid.clone())),
                Err(WorkerError::Protocol)
            );
        }
        assert_eq!(
            embed_text("猫犬", 2, |window| match window {
                "猫" => Ok(axis(0, 1.0)),
                "犬" => Ok(axis(0, -1.0)),
                _ => Err(WorkerError::InputOverflow),
            }),
            Err(WorkerError::Inference)
        );
    }
}
