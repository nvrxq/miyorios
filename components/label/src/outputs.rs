// набор полос по выходам, без Wayland: реальный composер и его конфигурация проверяются тестом 78
pub struct OutputBars<Id, Bar> {
    entries: Vec<(Id, Bar)>,
}

impl<Id, Bar> OutputBars<Id, Bar> {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    // только для тестов: продакшен-коду (main.rs) достаточно contains/find_mut/iter_mut
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Bar> {
        self.entries.iter_mut().map(|(_, bar)| bar)
    }

    pub fn find_mut(&mut self, mut matches: impl FnMut(&Bar) -> bool) -> Option<&mut Bar> {
        self.entries
            .iter_mut()
            .find(|(_, bar)| matches(bar))
            .map(|(_, bar)| bar)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    // композитор закрывает поверхность, а не выход: искать приходится по самой полосе, а не по id
    pub fn remove_matching(&mut self, mut matches: impl FnMut(&Bar) -> bool) -> Option<Bar> {
        let pos = self.entries.iter().position(|(_, bar)| matches(bar))?;
        Some(self.entries.remove(pos).1)
    }
}

impl<Id: PartialEq, Bar> OutputBars<Id, Bar> {
    pub fn contains(&self, id: &Id) -> bool {
        self.entries.iter().any(|(existing, _)| existing == id)
    }

    // false и без изменений, если id уже есть — повторный new_output не должен плодить вторую полосу
    pub fn insert(&mut self, id: Id, bar: Bar) -> bool {
        if self.contains(&id) {
            return false;
        }
        self.entries.push((id, bar));
        true
    }

    // удаление одного выхода не задевает записи остальных — они просто не совпадают по id
    pub fn remove(&mut self, id: &Id) -> Option<Bar> {
        let pos = self
            .entries
            .iter()
            .position(|(existing, _)| existing == id)?;
        Some(self.entries.remove(pos).1)
    }
}

impl<Id, Bar> Default for OutputBars<Id, Bar> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_adds_a_bar_per_output() {
        let mut bars = OutputBars::new();
        assert!(bars.insert("DP-1", 1));
        assert!(bars.insert("DP-2", 2));
        assert_eq!(bars.len(), 2);
    }

    #[test]
    fn repeated_new_output_on_the_same_output_does_not_duplicate_the_bar() {
        let mut bars = OutputBars::new();
        assert!(bars.insert("DP-1", 1));
        assert!(!bars.insert("DP-1", 99));
        assert_eq!(bars.len(), 1);
        assert!(bars.find_mut(|b| *b == 1).is_some());
        assert!(bars.find_mut(|b| *b == 99).is_none());
    }

    #[test]
    fn removing_one_output_does_not_touch_the_others() {
        let mut bars = OutputBars::new();
        bars.insert("DP-1", 1);
        bars.insert("DP-2", 2);
        bars.insert("DP-3", 3);

        assert_eq!(bars.remove(&"DP-2"), Some(2));

        assert_eq!(bars.len(), 2);
        assert!(bars.find_mut(|b| *b == 1).is_some());
        assert!(bars.find_mut(|b| *b == 3).is_some());
        assert!(bars.find_mut(|b| *b == 2).is_none());
    }

    #[test]
    fn removing_unknown_output_is_a_noop() {
        let mut bars: OutputBars<&str, i32> = OutputBars::new();
        bars.insert("DP-1", 1);
        assert_eq!(bars.remove(&"DP-9"), None);
        assert_eq!(bars.len(), 1);
    }

    // закрытая композитором поверхность уносит только свою полосу: остальные мониторы продолжают показывать
    #[test]
    fn remove_matching_takes_only_the_closed_bar() {
        let mut bars = OutputBars::new();
        bars.insert("DP-1", 1);
        bars.insert("DP-2", 2);
        assert_eq!(bars.remove_matching(|b| *b == 2), Some(2));
        assert_eq!(bars.len(), 1);
        assert!(!bars.is_empty());
        assert_eq!(bars.remove_matching(|b| *b == 2), None);
    }

    #[test]
    fn is_empty_only_when_no_bars_left() {
        let mut bars = OutputBars::new();
        assert!(bars.is_empty());
        bars.insert("DP-1", 1);
        assert!(!bars.is_empty());
        bars.remove(&"DP-1");
        assert!(bars.is_empty());
    }

    #[test]
    fn iter_mut_visits_every_bar() {
        let mut bars = OutputBars::new();
        bars.insert("DP-1", 1);
        bars.insert("DP-2", 2);
        for bar in bars.iter_mut() {
            *bar += 10;
        }
        let mut values: Vec<i32> = bars.iter_mut().map(|b| *b).collect();
        values.sort();
        assert_eq!(values, vec![11, 12]);
    }
}
