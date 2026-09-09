package main

// QuickSort sorts values in ascending order in place using quicksort.
func QuickSort(values []int) {
	quickSort(values, 0, len(values)-1)
}

func quickSort(values []int, low, high int) {
	for low < high {
		pivot := values[low+(high-low)/2]
		less, current, greater := low, low, high

		for current <= greater {
			switch {
			case values[current] < pivot:
				values[less], values[current] = values[current], values[less]
				less++
				current++
			case values[current] > pivot:
				values[current], values[greater] = values[greater], values[current]
				greater--
			default:
				current++
			}
		}

		// Recurse into the smaller partition first to keep stack usage bounded.
		if less-low < high-greater {
			quickSort(values, low, less-1)
			low = greater + 1
		} else {
			quickSort(values, greater+1, high)
			high = less - 1
		}
	}
}
