// ==================== SigV4 SIGNER ====================
export const SigV4 = {
  async sha256(data) {
    const buffer = typeof data === 'string' ? new TextEncoder().encode(data) : data;
    const hash = await crypto.subtle.digest('SHA-256', buffer);
    return this.hexEncode(hash);
  },

  async hmac(key, data) {
    const keyBuffer = typeof key === 'string' ? new TextEncoder().encode(key) : key;
    const dataBuffer = typeof data === 'string' ? new TextEncoder().encode(data) : data;
    const cryptoKey = await crypto.subtle.importKey('raw', keyBuffer, { name: 'HMAC', hash: 'SHA-256' }, false, ['sign']);
    return await crypto.subtle.sign('HMAC', cryptoKey, dataBuffer);
  },

  hexEncode(buffer) {
    return Array.from(new Uint8Array(buffer)).map(b => b.toString(16).padStart(2, '0')).join('');
  },

  uriEncode(str, encodeSlash = true) {
    let result = '';
    for (let i = 0; i < str.length; i++) {
      const c = str[i];
      if ((c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z') || (c >= '0' && c <= '9') || c === '-' || c === '.' || c === '_' || c === '~') {
        result += c;
      } else if (c === '/' && !encodeSlash) {
        result += '/';
      } else {
        const bytes = new TextEncoder().encode(c);
        for (const b of bytes) result += '%' + b.toString(16).toUpperCase().padStart(2, '0');
      }
    }
    return result;
  },

  async signingKey(secretKey, dateStamp, region, service) {
    const kDate = await this.hmac('AWS4' + secretKey, dateStamp);
    const kRegion = await this.hmac(kDate, region);
    const kService = await this.hmac(kRegion, service);
    return await this.hmac(kService, 'aws4_request');
  },

  async sign(method, url, headers, body, accessKey, secretKey, region = 'us-east-1', service = 's3') {
    const parsedUrl = new URL(url);
    const path = parsedUrl.pathname || '/';
    const queryString = parsedUrl.search ? parsedUrl.search.slice(1) : '';

    const now = new Date();
    const dateStamp = now.toISOString().replace(/[-:]/g, '').slice(0, 8);
    const amzDate = dateStamp + 'T' + now.toISOString().replace(/[-:]/g, '').slice(9, 15) + 'Z';

    // Compute payload hash
    let payloadHash;
    if (body instanceof ArrayBuffer || body instanceof Uint8Array) {
      payloadHash = await this.sha256(body);
    } else if (typeof body === 'string' && body.length > 0) {
      payloadHash = await this.sha256(body);
    } else if (body === null || body === undefined || body === '') {
      payloadHash = await this.sha256('');
    } else {
      // For File/Blob, use UNSIGNED-PAYLOAD to avoid reading entire file
      payloadHash = 'UNSIGNED-PAYLOAD';
    }

    // Set required headers
    headers['host'] = parsedUrl.host;
    headers['x-amz-date'] = amzDate;
    headers['x-amz-content-sha256'] = payloadHash;

    // Signed headers (sorted, lowercase)
    const signedHeaderNames = Object.keys(headers).map(h => h.toLowerCase()).sort();
    const signedHeadersStr = signedHeaderNames.join(';');

    // Canonical headers
    const canonicalHeaders = signedHeaderNames.map(name => {
      const val = headers[Object.keys(headers).find(k => k.toLowerCase() === name)];
      return name + ':' + val.trim().replace(/\s+/g, ' ') + '\n';
    }).join('');

    // Canonical query string
    let canonicalQueryString = '';
    if (queryString) {
      const params = [];
      for (const part of queryString.split('&')) {
        const eqIdx = part.indexOf('=');
        if (eqIdx >= 0) {
          params.push([decodeURIComponent(part.slice(0, eqIdx)), decodeURIComponent(part.slice(eqIdx + 1))]);
        } else {
          params.push([decodeURIComponent(part), '']);
        }
      }
      params.sort((a, b) => a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : a[1] < b[1] ? -1 : a[1] > b[1] ? 1 : 0);
      canonicalQueryString = params.map(([k, v]) => this.uriEncode(k, true) + '=' + this.uriEncode(v, true)).join('&');
    }

    // Canonical request
    const canonicalRequest = [
      method,
      path,  // S3: path used verbatim
      canonicalQueryString,
      canonicalHeaders,
      signedHeadersStr,
      payloadHash,
    ].join('\n');

    const canonicalRequestHash = await this.sha256(canonicalRequest);

    // Credential scope
    const scope = `${dateStamp}/${region}/${service}/aws4_request`;

    // String to sign
    const stringToSign = `AWS4-HMAC-SHA256\n${amzDate}\n${scope}\n${canonicalRequestHash}`;

    // Signing key and signature
    const key = await this.signingKey(secretKey, dateStamp, region, service);
    const signatureBuffer = await this.hmac(key, stringToSign);
    const signature = this.hexEncode(signatureBuffer);

    // Authorization header
    headers['Authorization'] = `AWS4-HMAC-SHA256 Credential=${accessKey}/${scope}, SignedHeaders=${signedHeadersStr}, Signature=${signature}`;

    return headers;
  }
};

// ==================== API CLIENT ====================
export function apiClient() {
  const getEndpoint = () => sessionStorage.getItem('arca_endpoint') || '';
  const getAccessKey = () => sessionStorage.getItem('arca_access_key') || '';
  const getSecretKey = () => sessionStorage.getItem('arca_secret_key') || '';
  // Encode each segment of an S3 key path, preserving '/' separators
  const encodeKeyPath = (key) => key.split('/').map(s => encodeURIComponent(s)).join('/');

  return {
    async request(method, path, { body = null, contentType = null, rawResponse = false, queryParams = {} } = {}) {
      const endpoint = getEndpoint();
      let url = endpoint + path;

      // Append query params
      const qp = Object.entries(queryParams).filter(([, v]) => v !== undefined);
      if (qp.length > 0) {
        const qs = qp.map(([k, v]) => v === '' ? encodeURIComponent(k) + '=' : encodeURIComponent(k) + '=' + encodeURIComponent(v)).join('&');
        url += (url.includes('?') ? '&' : '?') + qs;
      }

      const headers = {};
      if (contentType) headers['Content-Type'] = contentType;

      // Determine body for signing
      let signBody = body;
      if (body instanceof File || body instanceof Blob) {
        signBody = body; // Will use UNSIGNED-PAYLOAD
      }

      await SigV4.sign(method, url, headers, signBody, getAccessKey(), getSecretKey());

      const fetchOpts = { method, headers };
      if (body !== null && body !== undefined) fetchOpts.body = body;

      const resp = await fetch(url, fetchOpts);
      if (rawResponse) return resp;
      return resp;
    },

    // Admin API helpers (JSON)
    async adminGet(path) {
      const resp = await this.request('GET', '/admin' + path);
      if (!resp.ok) throw new Error(`Admin API error: ${resp.status}`);
      return resp.json();
    },

    async adminPost(path, data) {
      const resp = await this.request('POST', '/admin' + path, {
        body: JSON.stringify(data),
        contentType: 'application/json',
      });
      return resp;
    },

    async adminPut(path, data) {
      const opts = {};
      if (data !== undefined) {
        opts.body = JSON.stringify(data);
        opts.contentType = 'application/json';
      }
      return await this.request('PUT', '/admin' + path, opts);
    },

    async adminDelete(path, data) {
      const opts = {};
      if (data) {
        opts.body = JSON.stringify(data);
        opts.contentType = 'application/json';
      }
      return await this.request('DELETE', '/admin' + path, opts);
    },

    async adminArchive(bucket, keys) {
      return await this.request('POST', '/admin/archive', {
        body: JSON.stringify({ bucket, keys }),
        contentType: 'application/json',
        rawResponse: true,
      });
    },

    // S3 API helpers (XML)
    async s3ListBuckets() {
      const resp = await this.request('GET', '/');
      if (!resp.ok) throw new Error('Failed to list buckets');
      const xml = await resp.text();
      return this.parseListBuckets(xml);
    },

    async s3ListObjects(bucket, prefix = '', continuationToken = '') {
      const params = { 'list-type': '2', delimiter: '/', prefix };
      if (continuationToken) params['continuation-token'] = continuationToken;
      const resp = await this.request('GET', '/' + encodeURIComponent(bucket), { queryParams: params });
      if (!resp.ok) throw new Error('Failed to list objects');
      const xml = await resp.text();
      return this.parseListObjects(xml);
    },

    // List ALL objects under a prefix recursively (no delimiter).
    // Returns an array of {key, size, lastModified, etag} objects.
    async s3ListAllObjects(bucket, prefix = '') {
      const all = [];
      let token = '';
      do {
        const params = { 'list-type': '2', prefix };
        if (token) params['continuation-token'] = token;
        const resp = await this.request('GET', '/' + encodeURIComponent(bucket), { queryParams: params });
        if (!resp.ok) throw new Error('Failed to list objects');
        const xml = await resp.text();
        const result = this.parseListObjects(xml);
        for (const obj of result.objects) all.push(obj);
        token = result.isTruncated ? result.nextToken : '';
      } while (token);
      return all;
    },

    async s3CreateBucket(name) {
      return await this.request('PUT', '/' + encodeURIComponent(name));
    },

    async s3DeleteBucket(name) {
      return await this.request('DELETE', '/' + encodeURIComponent(name));
    },

    async s3PutObject(bucket, key, file) {
      const headers = {};
      if (file.type) headers['Content-Type'] = file.type;
      return await this.request('PUT', '/' + encodeURIComponent(bucket) + '/' + encodeKeyPath(key), { body: file });
    },

    async s3GetObject(bucket, key) {
      return await this.request('GET', '/' + encodeURIComponent(bucket) + '/' + encodeKeyPath(key), { rawResponse: true });
    },

    async s3HeadObject(bucket, key) {
      return await this.request('HEAD', '/' + encodeURIComponent(bucket) + '/' + encodeKeyPath(key));
    },

    async s3DeleteObject(bucket, key) {
      return await this.request('DELETE', '/' + encodeURIComponent(bucket) + '/' + encodeKeyPath(key));
    },

    async s3DeleteObjects(bucket, keys) {
      let xml = '<?xml version="1.0" encoding="UTF-8"?><Delete><Quiet>true</Quiet>';
      for (const key of keys) {
        xml += '<Object><Key>' + key.replace(/&/g, '&amp;').replace(/</g, '&lt;') + '</Key></Object>';
      }
      xml += '</Delete>';
      const resp = await this.request('POST', '/' + encodeURIComponent(bucket), {
        body: xml,
        contentType: 'application/xml',
        queryParams: { delete: '' },
      });
      if (!resp.ok) throw new Error('Delete objects failed');
      return resp;
    },

    // Multipart upload
    async s3CreateMultipartUpload(bucket, key) {
      const resp = await this.request('POST', '/' + encodeURIComponent(bucket) + '/' + encodeKeyPath(key), { queryParams: { uploads: '' } });
      if (!resp.ok) throw new Error('Failed to create multipart upload');
      const xml = await resp.text();
      const doc = new DOMParser().parseFromString(xml, 'text/xml');
      return doc.querySelector('UploadId')?.textContent || '';
    },

    async s3UploadPart(bucket, key, uploadId, partNumber, data) {
      const resp = await this.request('PUT', '/' + encodeURIComponent(bucket) + '/' + encodeKeyPath(key), {
        body: data,
        queryParams: { partNumber: String(partNumber), uploadId },
      });
      if (!resp.ok) throw new Error(`Failed to upload part ${partNumber}`);
      return resp.headers.get('ETag');
    },

    async s3CompleteMultipartUpload(bucket, key, uploadId, parts) {
      let xml = '<CompleteMultipartUpload>';
      for (const p of parts) {
        xml += `<Part><PartNumber>${p.partNumber}</PartNumber><ETag>${p.etag}</ETag></Part>`;
      }
      xml += '</CompleteMultipartUpload>';
      return await this.request('POST', '/' + encodeURIComponent(bucket) + '/' + encodeKeyPath(key), {
        body: xml,
        contentType: 'application/xml',
        queryParams: { uploadId },
      });
    },

    // Bucket encryption
    async s3GetBucketEncryption(bucket) {
      try {
        const resp = await this.request('GET', '/' + encodeURIComponent(bucket), { queryParams: { encryption: '' } });
        if (!resp.ok) return null;
        const xml = await resp.text();
        const doc = new DOMParser().parseFromString(xml, 'text/xml');
        const algo = doc.querySelector('SSEAlgorithm')?.textContent || null;
        return algo ? { algorithm: algo } : null;
      } catch { return null; }
    },

    async s3PutBucketEncryption(bucket) {
      const xml = '<ServerSideEncryptionConfiguration><Rule><ApplyServerSideEncryptionByDefault><SSEAlgorithm>AES256</SSEAlgorithm></ApplyServerSideEncryptionByDefault></Rule></ServerSideEncryptionConfiguration>';
      return await this.request('PUT', '/' + encodeURIComponent(bucket), {
        body: xml,
        contentType: 'application/xml',
        queryParams: { encryption: '' },
      });
    },

    async s3DeleteBucketEncryption(bucket) {
      return await this.request('DELETE', '/' + encodeURIComponent(bucket), { queryParams: { encryption: '' } });
    },

    async s3GetBucketVersioning(bucket) {
      return await this.request('GET', '/' + encodeURIComponent(bucket), { queryParams: { versioning: '' } });
    },

    async s3PutBucketVersioning(bucket, status) {
      const xml = `<VersioningConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Status>${status}</Status></VersioningConfiguration>`;
      return await this.request('PUT', '/' + encodeURIComponent(bucket), {
        body: xml,
        contentType: 'application/xml',
        queryParams: { versioning: '' },
      });
    },

    async s3GetBucketLifecycle(bucket) {
      return await this.request('GET', '/' + encodeURIComponent(bucket), { queryParams: { lifecycle: '' } });
    },

    async s3PutBucketLifecycle(bucket, xml) {
      return await this.request('PUT', '/' + encodeURIComponent(bucket), {
        body: xml,
        contentType: 'application/xml',
        queryParams: { lifecycle: '' },
      });
    },

    async s3DeleteBucketLifecycle(bucket) {
      return await this.request('DELETE', '/' + encodeURIComponent(bucket), { queryParams: { lifecycle: '' } });
    },

    async s3GetObjectLockConfiguration(bucket) {
      return await this.request('GET', '/' + encodeURIComponent(bucket), { queryParams: { 'object-lock': '' } });
    },

    async s3PutObjectLockConfiguration(bucket, xml) {
      return await this.request('PUT', '/' + encodeURIComponent(bucket), {
        body: xml,
        contentType: 'application/xml',
        queryParams: { 'object-lock': '' },
      });
    },

    async s3GetObjectRetention(bucket, key, versionId) {
      const path = '/' + encodeURIComponent(bucket) + '/' + encodeKeyPath(key);
      const qp = { retention: '' };
      if (versionId) qp.versionId = versionId;
      return await this.request('GET', path, { queryParams: qp });
    },

    async s3PutObjectRetention(bucket, key, xml, versionId) {
      const path = '/' + encodeURIComponent(bucket) + '/' + encodeKeyPath(key);
      const qp = { retention: '' };
      if (versionId) qp.versionId = versionId;
      return await this.request('PUT', path, { body: xml, contentType: 'application/xml', queryParams: qp });
    },

    async s3GetObjectLegalHold(bucket, key, versionId) {
      const path = '/' + encodeURIComponent(bucket) + '/' + encodeKeyPath(key);
      const qp = { 'legal-hold': '' };
      if (versionId) qp.versionId = versionId;
      return await this.request('GET', path, { queryParams: qp });
    },

    async s3PutObjectLegalHold(bucket, key, xml, versionId) {
      const path = '/' + encodeURIComponent(bucket) + '/' + encodeKeyPath(key);
      const qp = { 'legal-hold': '' };
      if (versionId) qp.versionId = versionId;
      return await this.request('PUT', path, { body: xml, contentType: 'application/xml', queryParams: qp });
    },

    async s3GetObjectTagging(bucket, key) {
      const path = '/' + encodeURIComponent(bucket) + '/' + encodeKeyPath(key);
      return await this.request('GET', path, { queryParams: { tagging: '' } });
    },

    async s3PutObjectTagging(bucket, key, xml) {
      const path = '/' + encodeURIComponent(bucket) + '/' + encodeKeyPath(key);
      return await this.request('PUT', path, {
        body: xml,
        contentType: 'application/xml',
        queryParams: { tagging: '' },
      });
    },

    async s3DeleteObjectTagging(bucket, key) {
      const path = '/' + encodeURIComponent(bucket) + '/' + encodeKeyPath(key);
      return await this.request('DELETE', path, { queryParams: { tagging: '' } });
    },

    async s3ListObjectVersions(bucket, prefix = '') {
      return await this.request('GET', '/' + encodeURIComponent(bucket), {
        queryParams: { versions: '', prefix, 'max-keys': '1000' },
      });
    },

    async s3GetObjectVersion(bucket, key, versionId) {
      const path = '/' + encodeURIComponent(bucket) + '/' + encodeKeyPath(key);
      return await this.request('GET', path, { rawResponse: true, queryParams: { versionId } });
    },

    async s3DeleteObjectVersion(bucket, key, versionId) {
      const path = '/' + encodeURIComponent(bucket) + '/' + encodeKeyPath(key);
      return await this.request('DELETE', path, { queryParams: { versionId } });
    },

    // XML parsing
    parseListBuckets(xml) {
      const doc = new DOMParser().parseFromString(xml, 'text/xml');
      const buckets = [];
      for (const b of doc.querySelectorAll('Bucket')) {
        buckets.push({
          name: b.querySelector('Name')?.textContent || '',
          creationDate: b.querySelector('CreationDate')?.textContent || '',
        });
      }
      return buckets;
    },

    parseListObjects(xml) {
      const doc = new DOMParser().parseFromString(xml, 'text/xml');
      const objects = [];
      for (const c of doc.querySelectorAll('Contents')) {
        objects.push({
          key: c.querySelector('Key')?.textContent || '',
          size: parseInt(c.querySelector('Size')?.textContent || '0', 10),
          lastModified: c.querySelector('LastModified')?.textContent || '',
          etag: c.querySelector('ETag')?.textContent || '',
        });
      }
      const directories = [];
      for (const cp of doc.querySelectorAll('CommonPrefixes')) {
        directories.push(cp.querySelector('Prefix')?.textContent || '');
      }
      const isTruncated = doc.querySelector('IsTruncated')?.textContent === 'true';
      const nextToken = doc.querySelector('NextContinuationToken')?.textContent || '';
      return { objects, directories, isTruncated, nextToken };
    },

    parseListVersions(xml) {
      const doc = new DOMParser().parseFromString(xml, 'text/xml');
      const entries = [];
      for (const v of doc.querySelectorAll('Version')) {
        entries.push({
          key: v.querySelector('Key')?.textContent || '',
          versionId: v.querySelector('VersionId')?.textContent || 'null',
          isLatest: v.querySelector('IsLatest')?.textContent === 'true',
          isDeleteMarker: false,
          size: parseInt(v.querySelector('Size')?.textContent || '0', 10),
          lastModified: v.querySelector('LastModified')?.textContent || '',
          etag: v.querySelector('ETag')?.textContent || '',
        });
      }
      for (const dm of doc.querySelectorAll('DeleteMarker')) {
        entries.push({
          key: dm.querySelector('Key')?.textContent || '',
          versionId: dm.querySelector('VersionId')?.textContent || 'null',
          isLatest: dm.querySelector('IsLatest')?.textContent === 'true',
          isDeleteMarker: true,
          size: 0,
          lastModified: dm.querySelector('LastModified')?.textContent || '',
          etag: '',
        });
      }
      // Sort: newest first.
      entries.sort((a, b) => new Date(b.lastModified) - new Date(a.lastModified));
      return entries;
    },

    // ── Notification Configuration ──

    async s3GetBucketNotification(bucket) {
      return await this.request('GET', '/' + encodeURIComponent(bucket), { queryParams: { notification: '' } });
    },

    async s3PutBucketNotification(bucket, xml) {
      return await this.request('PUT', '/' + encodeURIComponent(bucket), {
        body: xml,
        contentType: 'application/xml',
        queryParams: { notification: '' },
      });
    },
  };
}

export const api = apiClient();
