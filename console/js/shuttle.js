// Shuttle (dual-list transfer) component helpers.
// Each view creates its own shuttle state using these helpers.

/**
 * Creates shuttle state for managing a many-to-many association.
 * Merge the returned object into an Alpine data object.
 *
 * @param {string} prefix - Unique prefix for state keys (e.g. 'members', 'teamGrants')
 */
export function shuttleState(prefix) {
  return {
    [`${prefix}All`]: [],              // All items (left + right combined)
    [`${prefix}AssignedIds`]: [],       // IDs of assigned items (right list)
    [`${prefix}SelLeft`]: [],           // Selected IDs in left (available) list
    [`${prefix}SelRight`]: [],          // Selected IDs in right (assigned) list
    [`${prefix}LastLeft`]: null,        // Last clicked ID in left list (for shift-select)
    [`${prefix}LastRight`]: null,       // Last clicked ID in right list
    [`${prefix}DragSide`]: null,        // 'left' or 'right' during drag
    [`${prefix}DragId`]: null,          // ID being dragged
    [`${prefix}Moving`]: false,         // True during async move operations
  };
}

/**
 * Shuttle selection handler (click, shift+click, ctrl/cmd+click).
 *
 * @param {Array} selArray - The selection array to modify (e.g. this.membersSelLeft)
 * @param {Array} itemsList - The visible items list for range selection
 * @param {string} id - The clicked item's ID
 * @param {string} idField - The field name for the ID
 * @param {MouseEvent} event
 * @param {*} lastRef - Object with a `value` property for tracking last click
 */
export function toggleSelect(selArray, itemsList, id, idField, event, lastRef) {
  const ids = itemsList.map(i => i[idField]);

  if (event.shiftKey && lastRef.value !== null) {
    const start = ids.indexOf(lastRef.value);
    const end = ids.indexOf(id);
    if (start >= 0 && end >= 0) {
      const range = ids.slice(Math.min(start, end), Math.max(start, end) + 1);
      const merged = new Set([...selArray, ...range]);
      selArray.length = 0;
      selArray.push(...merged);
    }
  } else if (event.ctrlKey || event.metaKey) {
    const idx = selArray.indexOf(id);
    if (idx >= 0) selArray.splice(idx, 1);
    else selArray.push(id);
  } else {
    selArray.length = 0;
    selArray.push(id);
  }
  lastRef.value = id;
}
