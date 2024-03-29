use num_traits::NumAssign;
use num_traits::FromPrimitive;
use num_traits::ToPrimitive;
use timely::ExchangeData;
use num_traits::One;
use data::TrainingData;
use self::loss_functions::*;
use models::decision_tree::histogram_generics::*;
use models::decision_tree::tree::{DecisionTree, Rule, NodeIndex, Node};
use num_traits::{NumCast, Float};
use ordered_float::OrderedFloat;
use std::cmp::Ordering;
use std::collections::binary_heap::BinaryHeap;
use std::collections::BTreeMap;
use std::fmt;
use std::ops::Bound::{Excluded, Included, Unbounded};

pub mod loss_functions;
mod target_value_set;
pub use self::target_value_set::*;

/// Histogram describing the target value distribution at a certain tree node
#[derive(Clone)]
pub struct Histogram<L: Float, C: ExchangeData + NumAssign> {
    bins: BTreeMap<BinAddress<L>, BinData<L, C>>,
    distances: BinaryHeap<BinDistance<L>>,
    n_bins: usize,
}

#[derive(Clone, Abomonation)]
pub struct SerializableHistogram<L, C>{
    n_bins: usize,
    bins: Vec<(L, L, BinData<L, C>)>
}

impl<L: ContinuousValue, C: ExchangeData + NumAssign> BaseHistogram<L, C> for Histogram<L, C> {
    type Bin = (BinAddress<L>, BinData<L, C>);

    fn new(n_bins: usize) -> Self {
        Histogram {
            n_bins,
            distances: BinaryHeap::new(),
            bins: BTreeMap::new(),
        }
    }

    fn insert(&mut self, y: L, count: C) {
        let new_bin_data = BinData::init(y);
        let new_bin_address = BinAddress::init(y);
        let mut found = false;
        let before = self
            .bins
            .range_mut((Unbounded, Included(new_bin_address.clone())))
            .next_back()
            .and_then(|(addr, data)| {
                if addr.right >= new_bin_address.right {
                    data.count += count;
                    data.sum = data.sum + y;
                    found = true;
                    None
                } else {
                    Some(BinDistance::new(addr, &new_bin_address))
                }
            });

        if !found {
            if let Some(dist) = before { self.distances.push(dist); }
            if let Some(dist) = self.bins
                .range((Excluded(new_bin_address.clone()), Unbounded))
                .next()
                .map(|(addr, _)| BinDistance::new(&new_bin_address, addr)) {
                self.distances.push(dist);
            }
            self.bins.insert(new_bin_address, new_bin_data);
        }
        self.shrink_to_fit();
    }

    fn count(&self) -> C {
        self.bins.iter().fold(C::zero(), |sum, (_, d)| sum + d.count.clone())
    }
}

impl<L, C> Median<L> for Histogram<L, C>
where L: ContinuousValue,
    C: PartialOrd + NumAssign + ExchangeData + ToPrimitive + FromPrimitive,
{
    fn median(&self) -> Option<L> {
        let mut count_target = self.count() / C::from_usize(2).unwrap();
        let (bin_addr, bin_data) = self
            .bins
            .iter()
            .skip_while(|(_, data)| {
                if count_target >= data.count {
                    count_target -= data.count.clone();
                    true
                } else  { false }
            })
            .next()?;
        let ratio = L::from(count_target).unwrap() / L::from(bin_data.count.clone()).unwrap();
        let l = bin_addr.left.into_inner();
        let r = bin_addr.right.into_inner();
        Some(l + (r - l) * ratio)
    }
}

impl<L: ContinuousValue, C: NumAssign + ExchangeData> From<Histogram<L, C>> for SerializableHistogram<L, C> {
    /// Turn this item into a serializable version of itself
    fn from(hist: Histogram<L, C>) -> Self {
        let n_bins = hist.n_bins;
        let bins = hist
            .bins
            .into_iter()
            .map(|(address, data)| (address.left.into_inner(), address.right.into_inner(), data))
            .collect();
        SerializableHistogram { n_bins, bins }
    }
}

impl<L: ContinuousValue, C: NumAssign + ExchangeData> Into<Histogram<L, C>> for SerializableHistogram<L, C> {
    /// Recover a item from its serializable representation
    fn into(self) -> Histogram<L, C> {
        let mut histogram = Histogram::new(self.n_bins);
        for (left, right, data) in self.bins {
            histogram
                .bins
                .insert(BinAddress::new(left, right), data);
            histogram.rebuild_distances();
        }
        histogram
    }
}

impl<L: ContinuousValue, C: ExchangeData + NumAssign> HistogramSetItem for Histogram<L, C>
where SerializableHistogram<L, C>: Into<Histogram<L, C>> {
    type Serializable = SerializableHistogram<L, C>;
    
    /// Merge another instance of this type into this histogram
    fn merge(&mut self, other: Self) {
        for (new_addr, new_data) in other.bins {
            self.bins
                .entry(new_addr)
                .and_modify(|bin| bin.merge(&new_data))
                .or_insert(new_data);
        }
        self.rebuild_distances();
        self.shrink_to_fit();
    }

    fn merge_borrowed(&mut self, other: &Self) {
        for (new_addr, new_data) in &other.bins {
            self.bins
                .entry(new_addr.clone())
                .and_modify(|bin| bin.merge(&new_data))
                .or_insert_with(|| new_data.clone());
        }
        self.rebuild_distances();
        self.shrink_to_fit();
    }

    fn empty_clone(&self) -> Self {
        Self::new(self.n_bins)
    }
}

impl<L: Float, C: ExchangeData + NumAssign> Histogram<L, C>
where
    BinAddress<L>: Ord,
{
    fn shrink_to_fit(&mut self) {
        while self.bins.len() > self.n_bins {
            // find two closest together bins
            let least_diff = self.distances.pop().unwrap();

            let data_l = self.bins.remove(&least_diff.left);
            let data_r = self.bins.remove(&least_diff.right);

            // there may be "out of date" distances on the heap if one of the bins was merged
            if data_l.is_none() || data_r.is_none() {
                if let Some(l) = data_l {
                    self.bins.insert(least_diff.left, l);
                }
                if let Some(r) = data_r {
                    self.bins.insert(least_diff.right, r);
                }
                continue;
            }

            let (mut merged_addr, addr_r) = (least_diff.left, least_diff.right);
            merged_addr.merge(&addr_r);

            let mut merged_data = data_l.unwrap();
            merged_data.merge(&data_r.unwrap());
            self.bins.insert(merged_addr.clone(), merged_data);

            // insert updated distances after merge
            if let Some(dist) = self.bins
                .range((Excluded(&merged_addr), Unbounded))
                .next()
                .map(|(after_addr, _)| BinDistance::new(&merged_addr, after_addr)) {
                self.distances.push(dist);
            }

            if self.distances.len() > self.n_bins * 10 {
                self.rebuild_distances();
            }
        }
    }

    fn rebuild_distances(&mut self) {
        self.distances.clear();
        for (left, right) in self.bins.keys().zip(self.bins.keys().skip(1)) {
            self.distances.push(BinDistance::new(left, right));
        }
    }

    pub fn bins(&self) -> &BTreeMap<BinAddress<L>, BinData<L, C>> {
        &self.bins
    }
}

impl<L: Float + fmt::Debug, C: fmt::Debug + ExchangeData + NumAssign> fmt::Debug for Histogram<L, C> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        writeln!(fmt, "Bins:")?;
        for (addr, bin) in &self.bins {
            fmt::Display::fmt(
                &format!(
                    "{:?}/{:?} -> {:?}\n",
                    addr.left.into_inner(),
                    addr.right.into_inner(),
                    bin
                ),
                fmt,
            )?;
        }
        writeln!(fmt, "Distances:")?;
        for dist in &self.distances {
            fmt::Display::fmt(&format!("{:?}\n", dist), fmt)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct BinAddress<L: Float> {
    pub left: OrderedFloat<L>,
    pub right: OrderedFloat<L>,
}

impl<L: Float> Eq for BinAddress<L> {}

impl<L: Float> Ord for BinAddress<L> {
    fn cmp(&self, other: &Self) -> Ordering {
        let left_ord = self.left.cmp(&other.left);
        let right_ord = self.right.cmp(&other.right);
        left_ord.then(right_ord)
    }
}

impl<L: Float> PartialOrd for BinAddress<L> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<L: Float> BinAddress<L> {
    pub fn init(y: L) -> Self {
        BinAddress {
            left: y.into(),
            right: y.into(),
        }
    }

    pub fn new(left: L, right: L) -> Self {
        BinAddress {
            left: left.into(),
            right: right.into(),
        }
    }

    pub fn contains(&self, y: L) -> Ordering {
        let y_ord = OrderedFloat::from(y);
        if y_ord < self.left {
            Ordering::Less
        } else if y_ord > self.right {
            Ordering::Greater
        } else {
            Ordering::Equal
        }
    }

    /// Merges this bin with another one, summing the number of points
    /// and shifting the center of the bin to accomodate
    pub fn merge(&mut self, other: &Self) {
        self.left = OrderedFloat::min(self.left, other.left);
        self.right = OrderedFloat::max(self.right, other.right);
    }
}

#[derive(Debug, Clone, PartialEq, Abomonation)]
pub struct BinData<L, C> {
    count: C,
    sum: L,
}

impl<L: Float, C: NumAssign + ExchangeData> BinData<L, C> {
    pub fn init(y: L) -> Self {
        BinData { count: One::one(), sum: y }
    }

    pub fn new(count: C, sum: L) -> Self {
        BinData { count, sum }
    }

    /// Merges this bin with another one, summing the number of points
    /// and shifting the center of the bin to accomodate
    pub fn merge(&mut self, other: &Self) {
        self.sum = self.sum + other.sum;
        self.count += other.count.clone();
    }
}

trait PartialBinSum<L, C> {
    /// Estimates an R-partial sum of this bin, where R is any number
    /// between 0 and the number of samples contained in the bin
    /// panics if R is greater than the count of items in the bin
    fn partial_sum(&self, r: C) -> L;
}

impl<'a, 'b, L: Float + NumCast, C: Clone + PartialOrd + ToPrimitive + NumAssign> PartialBinSum<L, C> for (&'a BinAddress<L>, &'b BinData<L, C>) {
    /// Estimates an R-partial sum of this bin, where R is any number
    /// between 0 and the number of samples contained in the bin
    /// panics if R is greater than the count of items in the bin
    fn partial_sum(&self, r: C) -> L {
        let (addr, data) = self;
        let r_float = L::from(r.clone()).unwrap();
        if r < data.count {
            let count_float = L::from(data.count.clone()).unwrap();
            let two = L::from(2.).unwrap();
            let delta = (data.sum - *addr.right - count_float * *addr.left + *addr.left)
                / ((count_float - two) * (count_float - L::one()));
            r_float * *addr.left + r_float * (r_float - L::one()) * delta
        } else if r == data.count {
            data.sum
        } else {
            panic!("Attempt to calculate R-Partial sum where R > bin.count")
        }
    }
}

#[derive(Clone, Abomonation)]
struct BinDistance<L: Float> {
    pub left: BinAddress<L>,
    pub right: BinAddress<L>,
    pub distance: L,
}

impl<L: Float> BinDistance<L> {
    pub fn new(left: &BinAddress<L>, right: &BinAddress<L>) -> Self {
        BinDistance {
            left: left.clone(),
            right: right.clone(),
            distance: (right.left.into_inner() - left.right.into_inner()).max(L::from(0.).unwrap()),
        }
    }
}

// Implement reverse ordering for BinDistance so it can be used in a max-heap

impl<L: Float> PartialEq for BinDistance<L> {
    fn eq(&self, other: &Self) -> bool {
        self.distance == other.distance
    }
}

impl<L: Float> Eq for BinDistance<L> {}

impl<L: Float> PartialOrd for BinDistance<L> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.distance
            .partial_cmp(&other.distance)
            .and_then(|o| Some(o.reverse()))
    }
}

impl<L: Float> Ord for BinDistance<L> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.partial_cmp(other).unwrap_or(Ordering::Less)
    }
}

impl<L: Float + fmt::Debug> fmt::Debug for BinDistance<L> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        format!(
            "{:?}/{:?} -> {:?} -> {:?}/{:?}",
            self.left.left, self.left.right, self.distance, self.right.left, self.right.right
        ).fmt(fmt)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn insert() {
        let mut histogram = Histogram::new(3);
        let items = vec![1., 1., 2., 3.5, 2.1, 3.6];
        for i in items {
            histogram.insert(i, 1);
        }
        assert_eq!(
            histogram.bins().iter().collect::<Vec<_>>(),
            vec![
                (&BinAddress::new(1.0, 1.0), &BinData::new(2, 2.0)),
                (&BinAddress::new(2.0, 2.1), &BinData::new(2, 4.1)),
                (&BinAddress::new(3.5, 3.6), &BinData::new(2, 7.1)),
            ]
        )
    }

    #[test]
    fn merge() {
        let mut h1 = Histogram::new(3);
        vec![1., 1.5, 3., 4., 4.5, 6.]
            .into_iter()
            .for_each(|i| h1.insert(i, 1));

        let mut h2 = Histogram::new(3);
        vec![1.0, 7.0, 5.0].into_iter().for_each(|i| h2.insert(i, 1));
        h1.merge_borrowed(&h2);

        assert_eq!(
            h1.bins().iter().collect::<Vec<_>>(),
            vec![
                (&BinAddress::new(1.0, 3.0), &BinData::new(4, 6.5)),
                (&BinAddress::new(4.0, 5.0), &BinData::new(3, 13.5)),
                (&BinAddress::new(6.0, 7.0), &BinData::new(2, 13.0)),
            ]
        )
    }
}
