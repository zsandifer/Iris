import './env.mjs';
import test from 'node:test';
import assert from 'node:assert/strict';
import { photos } from '../../web/js/feed.js';

test('pipeline photos use checked local copies in the feed and Saved', () => {
  const originals = ['https://example.com/original.jpg'];
  assert.deepEqual(photos({ images: originals, photos: ['/api/feed/images/photo.jpg'] }), ['/api/feed/images/photo.jpg']);
  assert.deepEqual(photos({ images: originals, image_url: originals[0], photos: [] }), [], 'an empty checked-photo list never falls back to a third-party request');
  assert.deepEqual(photos({ images: originals }), originals, 'older agent posts still show their source photos');
});
