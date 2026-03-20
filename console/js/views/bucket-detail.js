import { api } from '../api.js';
import { formatBytes, ringColors, icons } from '../app.js';

// Squarified treemap algorithm
function squarify(items, x, y, w, h) {
  if (items.length === 0) return [];
  if (items.length === 1) return [{ ...items[0], x, y, w, h }];

  const total = items.reduce((s, i) => s + i.size, 0);
  const rects = [];

  let remaining = [...items];
  let cx = x, cy = y, cw = w, ch = h;

  while (remaining.length > 0) {
    const isWide = cw >= ch;
    const side = isWide ? ch : cw;
    const totalRemaining = remaining.reduce((s, i) => s + i.size, 0);

    // Find best row
    let row = [remaining[0]];
    let rowArea = (remaining[0].size / totalRemaining) * cw * ch;
    let bestRatio = worstRatio(row, side, totalRemaining, cw * ch);

    for (let i = 1; i < remaining.length; i++) {
      const testRow = [...row, remaining[i]];
      const testRatio = worstRatio(testRow, side, totalRemaining, cw * ch);
      if (testRatio <= bestRatio) {
        row = testRow;
        bestRatio = testRatio;
      } else {
        break;
      }
    }

    // Lay out row
    const rowTotalSize = row.reduce((s, i) => s + i.size, 0);
    const rowFraction = rowTotalSize / totalRemaining;

    if (isWide) {
      const rowWidth = cw * rowFraction;
      let ry = cy;
      for (const item of row) {
        const itemHeight = ch * (item.size / rowTotalSize);
        rects.push({ ...item, x: cx, y: ry, w: rowWidth, h: itemHeight });
        ry += itemHeight;
      }
      cx += rowWidth;
      cw -= rowWidth;
    } else {
      const rowHeight = ch * rowFraction;
      let rx = cx;
      for (const item of row) {
        const itemWidth = cw * (item.size / rowTotalSize);
        rects.push({ ...item, x: rx, y: cy, w: itemWidth, h: rowHeight });
        rx += itemWidth;
      }
      cy += rowHeight;
      ch -= rowHeight;
    }

    remaining = remaining.slice(row.length);
  }

  return rects;
}

function worstRatio(row, side, total, area) {
  const rowTotal = row.reduce((s, i) => s + i.size, 0);
  const rowArea = (rowTotal / total) * area;
  const rowSide = rowArea / side;
  let worst = 0;
  for (const item of row) {
    const itemArea = (item.size / total) * area;
    const itemSide = itemArea / rowSide;
    const ratio = Math.max(itemSide / rowSide, rowSide / itemSide);
    worst = Math.max(worst, ratio);
  }
  return worst;
}

// ==================== BUCKET DETAIL VIEW ====================
export function bucketDetailView() {
  return {
    bucketName: '',
    prefix: '',
    objects: [],
    directories: [],
    loading: true,
    selectedObject: null,
    versionsExpanded: false,
    versions: [],
    versionsLoading: false,
    versionsError: '',
    showDeleteVersionModal: false,
    deleteVersionKey: '',
    deleteVersionId: '',
    deleteVersionIsMarker: false,
    deleteVersionIsLatest: false,
    deletingVersion: false,
    icons,
    encryptionEnabled: false,
    bucketEncrypted: false,
    bucketVersioned: false,
    kmsProvider: null,
    showTreemap: false,
    treemapRects: [],
    dragActive: false,
    uploading: false,
    uploadFileName: '',
    uploadProgress: 0,
    showDeleteObjModal: false,
    deleteObjKey: '',
    deleteIsDir: false,
    deleteRecursive: false,
    deleteError: '',
    deleteProgress: '',
    deleting: false,
    showCreateFolderModal: false,
    newFolderName: '',
    createFolderError: '',
    selectedKeys: new Set(),
    archiving: false,
    showDeleteBatchModal: false,
    deleteBatchFileCount: 0,
    deleteBatchDirCount: 0,
    showShareModal: false,
    shareKey: '',
    shareExpiry: 3600,
    shareExpiryOptions: [
      { label: '1 hour', value: 3600 },
      { label: '6 hours', value: 21600 },
      { label: '1 day', value: 86400 },
      { label: '7 days', value: 604800 },
    ],
    shareUrl: '',
    shareExpiresAt: '',
    shareError: '',
    shareGenerating: false,
    shareCopied: false,
    ringColors,

    get prefixParts() {
      if (!this.prefix) return [];
      return this.prefix.split('/').filter(Boolean);
    },

    prefixUpTo(index) {
      return this.prefixParts.slice(0, index + 1).join('/') + '/';
    },

    async load() {
      // Parse bucket name and prefix from current hash
      const hash = window.location.hash || '';
      if (hash.startsWith('#/buckets/')) {
        this.bucketName = decodeURIComponent(hash.slice('#/buckets/'.length).split('?')[0]);
        const prefixMatch = hash.match(/[?&]prefix=([^&]*)/);
        this.prefix = prefixMatch ? decodeURIComponent(prefixMatch[1]) : '';
      }
      if (!this.bucketName) { this.loading = false; return; }
      this.loading = true;
      // Fetch admin info and per-bucket encryption status (best-effort)
      try {
        const info = await api.adminGet('/info');
        this.encryptionEnabled = !!info.encryption_enabled;
        this.kmsProvider = info.kms_provider || null;
      } catch {}
      try {
        const enc = await api.s3GetBucketEncryption(this.bucketName);
        this.bucketEncrypted = this.encryptionEnabled || !!enc;
      } catch {
        this.bucketEncrypted = this.encryptionEnabled;
      }
      try {
        const vResp = await api.s3GetBucketVersioning(this.bucketName);
        if (vResp.ok) {
          const xml = await vResp.text();
          const m = xml.match(/<Status>(.*?)<\/Status>/);
          this.bucketVersioned = m ? m[1] : false;
        }
      } catch {}
      const bucket = this.bucketName;
      try {
        const result = await api.s3ListObjects(bucket, this.prefix);
        // Filter out directory marker objects (zero-byte objects with keys
        // ending in '/') — they represent folders, not real files
        this.objects = result.objects.filter(o => !o.key.endsWith('/'));
        this.directories = result.directories;
        if (this.showTreemap) this.computeTreemap();
      } catch (e) { console.error(e); }
      this.loading = false;
    },

    get allSelected() {
      if (this.objects.length === 0 && this.directories.length === 0) return false;
      for (const obj of this.objects) {
        if (!this.selectedKeys.has(obj.key)) return false;
      }
      for (const dir of this.directories) {
        if (!this.selectedKeys.has('dir:' + dir)) return false;
      }
      return true;
    },

    toggleSelect(key) {
      const next = new Set(this.selectedKeys);
      if (next.has(key)) next.delete(key); else next.add(key);
      this.selectedKeys = next;
    },

    toggleSelectAll() {
      if (this.allSelected) {
        this.selectedKeys = new Set();
      } else {
        const next = new Set();
        for (const obj of this.objects) next.add(obj.key);
        for (const dir of this.directories) next.add('dir:' + dir);
        this.selectedKeys = next;
      }
    },

    navigatePrefix(newPrefix) {
      this.prefix = newPrefix;
      let hash = '#/buckets/' + encodeURIComponent(this.bucketName);
      if (newPrefix) hash += '?prefix=' + encodeURIComponent(newPrefix);
      window.location.hash = hash;
      this.selectedObject = null;
      this.selectedKeys = new Set();
      this.$nextTick(() => this.load());
    },

    dirName(dir) {
      const parts = dir.replace(/\/$/, '').split('/');
      return parts[parts.length - 1] + '/';
    },

    fileName(key) {
      const parts = key.split('/');
      return parts[parts.length - 1];
    },

    async selectObject(obj) {
      this.selectedObject = { ...obj, encrypted: false };
      this.versionsExpanded = false;
      this.versions = [];
      this.versionsError = '';
      try {
        const resp = await api.s3HeadObject(this.bucketName, obj.key);
        if (resp.headers.get('x-amz-server-side-encryption')) {
          this.selectedObject = { ...this.selectedObject, encrypted: true };
        }
      } catch {}
    },

    async toggleVersions() {
      this.versionsExpanded = !this.versionsExpanded;
      if (this.versionsExpanded && this.versions.length === 0) {
        await this.loadVersions();
      }
    },

    async loadVersions() {
      if (!this.selectedObject) return;
      this.versionsLoading = true;
      this.versionsError = '';
      try {
        const resp = await api.s3ListObjectVersions(this.bucketName, this.selectedObject.key);
        if (!resp.ok) throw new Error(`Error ${resp.status}`);
        const xml = await resp.text();
        const all = api.parseListVersions(xml);
        // Filter to only this exact key (prefix match might return more).
        this.versions = all.filter(v => v.key === this.selectedObject.key);
      } catch (e) {
        this.versionsError = e.message;
      }
      this.versionsLoading = false;
    },

    async downloadVersion(key, versionId) {
      try {
        const resp = await api.s3GetObjectVersion(this.bucketName, key, versionId);
        if (!resp.ok) throw new Error(`Error ${resp.status}`);
        const blob = await resp.blob();
        const url = URL.createObjectURL(blob);
        const a = document.createElement('a');
        a.href = url;
        a.download = this.fileName(key);
        a.click();
        URL.revokeObjectURL(url);
      } catch (e) {
        this.$dispatch('show-toast', { message: 'Download failed: ' + e.message, type: 'error' });
      }
    },

    confirmDeleteVersion(key, versionId, isDeleteMarker, isLatest) {
      this.deleteVersionKey = key;
      this.deleteVersionId = versionId;
      this.deleteVersionIsMarker = isDeleteMarker;
      this.deleteVersionIsLatest = isLatest;
      this.deletingVersion = false;
      this.showDeleteVersionModal = true;
    },

    async performDeleteVersion() {
      this.deletingVersion = true;
      try {
        const resp = await api.s3DeleteObjectVersion(this.bucketName, this.deleteVersionKey, this.deleteVersionId);
        if (!resp.ok && resp.status !== 204) throw new Error(`Error ${resp.status}`);
        this.showDeleteVersionModal = false;
        await this.loadVersions();
        await this.loadObjects();
      } catch (e) {
        this.$dispatch('show-toast', { message: 'Delete failed: ' + e.message, type: 'error' });
      }
    },

    truncateVersionId(vid) {
      if (!vid || vid === 'null') return 'null';
      return vid.substring(0, 8);
    },

    async downloadObject(key) {
      const bucket = this.bucketName;
      try {
        const resp = await api.s3GetObject(bucket, key);
        const blob = await resp.blob();
        const url = URL.createObjectURL(blob);
        const a = document.createElement('a');
        a.href = url;
        a.download = this.fileName(key);
        a.click();
        URL.revokeObjectURL(url);
      } catch (e) {
        this.$dispatch('show-toast', { message: 'Download failed: ' + e.message, type: 'error' });
      }
    },

    confirmDeleteObject(key) {
      this.deleteObjKey = key;
      this.deleteIsDir = false;
      this.deleteRecursive = false;
      this.deleteError = '';
      this.deleteProgress = '';
      this.showDeleteObjModal = true;
    },

    confirmDeleteDir(dir) {
      this.deleteObjKey = dir;
      this.deleteIsDir = true;
      this.deleteRecursive = false;
      this.deleteError = '';
      this.deleteProgress = '';
      this.showDeleteObjModal = true;
    },

    async deleteObject() {
      const bucket = this.bucketName;
      this.deleteError = '';
      this.deleteProgress = '';
      this.deleting = true;
      try {
        if (this.deleteIsDir && this.deleteRecursive) {
          // List all objects under the prefix and delete them in batches
          let token = '';
          let totalDeleted = 0;
          do {
            const result = await api.s3ListObjects(bucket, this.deleteObjKey, token);
            const keys = result.objects.map(o => o.key);
            // Also include the directory marker itself
            if (!token && keys.indexOf(this.deleteObjKey) === -1) {
              keys.push(this.deleteObjKey);
            }
            // Recursively list subdirectories too
            for (const dir of result.directories) {
              keys.push(dir); // directory marker
            }
            if (keys.length > 0) {
              await api.s3DeleteObjects(bucket, keys);
              totalDeleted += keys.length;
              this.deleteProgress = `Deleted ${totalDeleted} objects...`;
            }
            token = result.isTruncated ? result.nextToken : '';
          } while (token);
          // Final pass: re-list to catch any remaining objects (from nested dirs)
          let remaining = true;
          while (remaining) {
            const result = await api.s3ListObjects(bucket, this.deleteObjKey);
            const keys = result.objects.map(o => o.key);
            for (const dir of result.directories) keys.push(dir);
            if (keys.length === 0) { remaining = false; break; }
            await api.s3DeleteObjects(bucket, keys);
            totalDeleted += keys.length;
            this.deleteProgress = `Deleted ${totalDeleted} objects...`;
          }
          // Delete the directory marker itself
          await api.s3DeleteObject(bucket, this.deleteObjKey);
        } else {
          const resp = await api.s3DeleteObject(bucket, this.deleteObjKey);
          if (!resp.ok) throw new Error('Delete failed');
        }
        this.showDeleteObjModal = false;
        this.selectedObject = null;
        await this.load();
      } catch (e) {
        this.deleteError = e.message;
      }
      this.deleting = false;
    },

    async createFolder() {
      this.createFolderError = '';
      const name = this.newFolderName.replace(/^\/+|\/+$/g, '').trim();
      if (!name) { this.createFolderError = 'Folder name is required.'; return; }
      const key = this.prefix + name + '/';
      try {
        const resp = await api.s3PutObject(this.bucketName, key, new Blob([], { type: 'application/x-directory' }));
        if (!resp.ok) throw new Error('Failed to create folder');
        this.showCreateFolderModal = false;
        this.newFolderName = '';
        await this.load();
      } catch (e) {
        this.createFolderError = e.message;
      }
    },

    async uploadFiles(files) {
      if (!files || files.length === 0) return;
      const bucket = this.bucketName;
      const prefix = this.prefix;

      for (const file of files) {
        // webkitRelativePath is set when uploading a folder (e.g. "mydir/sub/file.txt").
        // For regular file uploads it's empty, so we fall back to file.name.
        const relativePath = file.webkitRelativePath || file.name;
        const key = prefix + relativePath;
        this.uploading = true;
        this.uploadFileName = relativePath;
        this.uploadProgress = 0;

        try {
          const MULTIPART_THRESHOLD = 100 * 1024 * 1024; // 100MB
          if (file.size > MULTIPART_THRESHOLD) {
            await this.multipartUpload(bucket, key, file);
          } else {
            await api.s3PutObject(bucket, key, file);
            this.uploadProgress = 100;
          }
        } catch (e) {
          console.error('Upload failed:', e);
          this.$dispatch('show-toast', { message: 'Upload failed: ' + e.message, type: 'error' });
        }
      }

      this.uploading = false;
      await this.load();
    },

    async multipartUpload(bucket, key, file) {
      const PART_SIZE = 5 * 1024 * 1024; // 5MB
      const uploadId = await api.s3CreateMultipartUpload(bucket, key);
      const parts = [];
      const totalParts = Math.ceil(file.size / PART_SIZE);

      for (let i = 0; i < totalParts; i++) {
        const start = i * PART_SIZE;
        const end = Math.min(start + PART_SIZE, file.size);
        const chunk = file.slice(start, end);
        const data = await chunk.arrayBuffer();

        const etag = await api.s3UploadPart(bucket, key, uploadId, i + 1, data);
        parts.push({ partNumber: i + 1, etag: etag });
        this.uploadProgress = Math.round(((i + 1) / totalParts) * 100);
      }

      await api.s3CompleteMultipartUpload(bucket, key, uploadId, parts);
    },

    // Drag-and-drop handler that supports both files and directories.
    async handleDrop(event) {
      const items = event.dataTransfer.items;
      if (!items) {
        // Fallback: no DataTransferItem API, use plain files.
        this.uploadFiles(event.dataTransfer.files);
        return;
      }
      const files = [];
      const promises = [];
      for (const item of items) {
        const entry = item.webkitGetAsEntry ? item.webkitGetAsEntry() : null;
        if (entry) {
          promises.push(this._traverseEntry(entry, '', files));
        }
      }
      await Promise.all(promises);
      if (files.length > 0) {
        await this.uploadFiles(files);
      }
    },

    // Recursively traverse a FileSystemEntry tree, collecting File objects
    // with their webkitRelativePath set manually via a wrapper.
    async _traverseEntry(entry, basePath, files) {
      if (entry.isFile) {
        const file = await new Promise((resolve, reject) => entry.file(resolve, reject));
        // Create a wrapper that exposes webkitRelativePath.
        const relativePath = basePath ? basePath + '/' + file.name : file.name;
        Object.defineProperty(file, 'webkitRelativePath', { value: relativePath, writable: false });
        files.push(file);
      } else if (entry.isDirectory) {
        const dirPath = basePath ? basePath + '/' + entry.name : entry.name;
        const reader = entry.createReader();
        const entries = await new Promise((resolve, reject) => {
          const all = [];
          const readBatch = () => {
            reader.readEntries(batch => {
              if (batch.length === 0) { resolve(all); return; }
              all.push(...batch);
              readBatch();
            }, reject);
          };
          readBatch();
        });
        for (const child of entries) {
          await this._traverseEntry(child, dirPath, files);
        }
      }
    },

    confirmDeleteSelected() {
      if (this.selectedKeys.size === 0) return;
      let dirs = 0, files = 0;
      for (const key of this.selectedKeys) {
        if (key.startsWith('dir:')) dirs++;
        else files++;
      }
      this.deleteBatchDirCount = dirs;
      this.deleteBatchFileCount = files;
      this.showDeleteBatchModal = true;
    },

    async deleteSelected() {
      if (this.selectedKeys.size === 0) return;
      const bucket = this.bucketName;
      this.deleting = true;
      try {
        // Collect all object keys to delete.
        const allKeys = [];
        for (const key of this.selectedKeys) {
          if (key.startsWith('dir:')) {
            // List ALL objects under this prefix (recursive, no delimiter).
            const objects = await api.s3ListAllObjects(bucket, key.slice(4));
            for (const obj of objects) allKeys.push(obj.key);
          } else {
            allKeys.push(key);
          }
        }
        // Delete in batches of 1000 (S3 limit).
        for (let i = 0; i < allKeys.length; i += 1000) {
          const batch = allKeys.slice(i, i + 1000);
          await api.s3DeleteObjects(bucket, batch);
        }
        this.selectedKeys = new Set();
        this.selectedObject = null;
        await this.load();
      } catch (e) {
        this.$dispatch('show-toast', { message: 'Delete failed: ' + e.message, type: 'error' });
      }
      this.deleting = false;
    },

    async downloadSelected() {
      if (this.selectedKeys.size === 0) return;
      this.archiving = true;
      try {
        // Collect all object keys (for directories, list recursively).
        const bucket = this.bucketName;
        const allKeys = [];
        for (const key of this.selectedKeys) {
          if (key.startsWith('dir:')) {
            const objects = await api.s3ListAllObjects(bucket, key.slice(4));
            for (const obj of objects) {
              // Skip directory markers (zero-byte objects with key ending in '/').
              if (!obj.key.endsWith('/')) allKeys.push(obj.key);
            }
          } else {
            allKeys.push(key);
          }
        }
        if (allKeys.length === 0) {
          this.$dispatch('show-toast', { message: 'No objects to download', type: 'error' });
          this.archiving = false;
          return;
        }
        const resp = await api.adminArchive(bucket, allKeys);
        if (!resp.ok) {
          const text = await resp.text();
          throw new Error(text);
        }
        const blob = await resp.blob();
        const url = URL.createObjectURL(blob);
        const a = document.createElement('a');
        a.href = url;
        a.download = bucket + '.tar.gz';
        a.click();
        URL.revokeObjectURL(url);
      } catch (e) {
        this.$dispatch('show-toast', { message: 'Download failed: ' + e.message, type: 'error' });
      }
      this.archiving = false;
    },

    // Treemap (squarified layout)
    computeTreemap() {
      const items = this.objects.filter(o => o.size > 0).map(o => ({ obj: o, size: o.size }));
      if (items.length === 0) { this.treemapRects = []; return; }
      items.sort((a, b) => b.size - a.size);
      this.treemapRects = squarify(items, 0, 0, 800, 400);
    },

    renderTreemap() {
      if (this.treemapRects.length === 0) return '';
      let rects = '';
      for (let i = 0; i < this.treemapRects.length; i++) {
        const r = this.treemapRects[i];
        const color = ringColors[i % ringColors.length];
        const label = (r.w > 50 && r.h > 20) ? `<text x="${r.x + r.w/2}" y="${r.y + r.h/2}" text-anchor="middle" dominant-baseline="middle" fill="white" font-size="10" font-family="DM Sans, system-ui">${this.fileName(r.obj.key)}</text>` : '';
        rects += `<g class="treemap-rect" style="cursor:pointer"><rect x="${r.x}" y="${r.y}" width="${r.w}" height="${r.h}" fill="${color}" rx="3" opacity="0.7"/>${label}</g>`;
      }
      return `<svg viewBox="0 0 800 400" class="w-full" style="height:350px">${rects}</svg>`;
    },

    openShareModal(key) {
      this.shareKey = key;
      this.shareUrl = '';
      this.shareExpiresAt = '';
      this.shareError = '';
      this.shareGenerating = false;
      this.shareCopied = false;
      this.shareExpiry = 3600;
      this.showShareModal = true;
    },

    async generateShareUrl() {
      this.shareGenerating = true;
      this.shareError = '';
      try {
        const resp = await api.adminPost('/presign', {
          bucket: this.bucketName,
          key: this.shareKey,
          expires: this.shareExpiry,
          endpoint: sessionStorage.getItem('arca_endpoint') || undefined,
        });
        if (!resp.ok) {
          const text = await resp.text();
          throw new Error(text || `HTTP ${resp.status}`);
        }
        const body = await resp.json();
        this.shareUrl = body.url;
        this.shareExpiresAt = body.expires_at ? new Date(body.expires_at).toLocaleString() : '';
      } catch (e) {
        this.shareError = 'Failed to generate link: ' + e.message;
      }
      this.shareGenerating = false;
    },

    async copyShareUrl() {
      try {
        await navigator.clipboard.writeText(this.shareUrl);
        this.shareCopied = true;
        setTimeout(() => this.shareCopied = false, 2000);
      } catch {
        // Fallback for non-HTTPS contexts
        const el = document.createElement('textarea');
        el.value = this.shareUrl;
        document.body.appendChild(el);
        el.select();
        document.execCommand('copy');
        document.body.removeChild(el);
        this.shareCopied = true;
        setTimeout(() => this.shareCopied = false, 2000);
      }
    },

    formatBytes,
  };
}
